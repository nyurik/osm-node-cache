//! What a command reads: which objects (by type, ID and tag), which changes (by action, changeset and user), and on
//! how many threads.

use std::error::Error;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use clap::builder::{PossibleValuesParser, TypedValueParser};
use osmflow::osc::{Action, Change};
use osmflow::rayon::ThreadPoolBuilder;
use osmflow::{Block, MemberType, PbfError, PbfReader, Select, TagMode};

use crate::input::{Seqs, Since, parse_seqs, parse_since};

/// A flag's parser of `T`'s names, as strum gives them, which the help and the errors list.
pub fn names<T>() -> impl TypedValueParser<Value = T>
where
    T: strum::VariantNames
        + FromStr<Err: Error + Send + Sync + 'static>
        + Clone
        + Send
        + Sync
        + 'static,
{
    PossibleValuesParser::new(T::VARIANTS).try_map(|s| s.parse::<T>())
}

/// The name of the first flag of `flags` that is given.
pub fn first_given(flags: &[(bool, &'static str)]) -> Option<&'static str> {
    flags
        .iter()
        .find_map(|&(given, name)| given.then_some(name))
}

/// An object type, as the flags name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum Kind {
    Node,
    Way,
    Relation,
}

impl From<MemberType> for Kind {
    fn from(kind: MemberType) -> Self {
        match kind {
            MemberType::Node => Self::Node,
            MemberType::Way => Self::Way,
            MemberType::Relation => Self::Relation,
        }
    }
}

impl Kind {
    /// The types in the order files hold them, which [`index`](Self::index) counts.
    pub const ALL: [Self; 3] = [Self::Node, Self::Way, Self::Relation];

    pub fn index(self) -> usize {
        self as usize
    }

    /// The name of many: `nodes`, as summaries label them.
    pub fn plural(self) -> &'static str {
        match self {
            Self::Node => "nodes",
            Self::Way => "ways",
            Self::Relation => "relations",
        }
    }
}

/// An `--id` value: an ID of one type, or of any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdSpec {
    kind: Option<Kind>,
    id: i64,
}

fn parse_id(s: &str) -> Result<IdSpec> {
    let (kind, number) = match s.as_bytes().first() {
        Some(b'n') => (Some(Kind::Node), &s[1..]),
        Some(b'w') => (Some(Kind::Way), &s[1..]),
        Some(b'r') => (Some(Kind::Relation), &s[1..]),
        _ => (None, s),
    };
    let id = number.parse().with_context(|| {
        format!("{s:?} is not an ID: a number, with n, w or r before it for one type")
    })?;
    Ok(IdSpec { kind, id })
}

/// A `--tag` value: a key, and the values it must have (any, if none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSpec {
    pub key: String,
    pub values: Vec<String>,
}

pub fn parse_tag(s: &str) -> Result<TagSpec> {
    let (key, values) = match s.split_once('=') {
        Some((key, values)) => (key, values.split(',').map(str::to_owned).collect()),
        None => (s, Vec::new()),
    };
    if key.is_empty() {
        bail!("{s:?} has no key");
    }
    Ok(TagSpec {
        key: key.to_owned(),
        values,
    })
}

/// The flags that choose objects.
#[derive(Debug, clap::Args)]
pub struct Filter {
    /// Only objects with these IDs, by commas.
    ///
    /// `12` is an ID of any type, and `n12`, `w12` or `r12` one of a node, a way or a relation. The flag can also be
    /// repeated.
    #[arg(long = "id", value_name = "ID", value_delimiter = ',', value_parser = parse_id)]
    ids: Vec<IdSpec>,
    /// Only objects of these types, by commas.
    #[arg(short = 't', long = "type", value_name = "TYPE", value_delimiter = ',')]
    types: Vec<Kind>,
    /// Only objects with this tag; repeated, with any of them.
    ///
    /// `KEY` matches any value, and `KEY=VALUE` or `KEY=V1,V2` one of the values. PBF blocks without the key are
    /// skipped unread.
    #[arg(long = "tag", value_name = "KEY[=VALUES]", value_parser = parse_tag)]
    tags: Vec<TagSpec>,
}

impl Filter {
    /// Whether the flags pick objects by ID or tag, rather than taking every object of the chosen types.
    pub fn picks(&self) -> bool {
        !self.ids.is_empty() || !self.tags.is_empty()
    }

    /// Whether objects of `kind` may be wanted: of a chosen type, and, when every `--id` names a type, of one of those.
    pub fn wants(&self, kind: Kind) -> bool {
        (self.types.is_empty() || self.types.contains(&kind))
            && (self.ids.is_empty() || self.ids.iter().any(|s| s.kind.is_none_or(|k| k == kind)))
    }

    /// `select` without the types no object is wanted of.
    pub fn select<T: TagMode>(&self, mut select: Select<T>) -> Select<T> {
        if !self.wants(Kind::Node) {
            select = select.without_nodes();
        }
        if !self.wants(Kind::Way) {
            select = select.without_ways();
        }
        if !self.wants(Kind::Relation) {
            select = select.without_relations();
        }
        select
    }

    /// `file`, skipping the blocks that have none of the `--tag` keys.
    pub fn skip_blocks(&self, file: PbfReader) -> PbfReader {
        if self.tags.is_empty() {
            return file;
        }
        let keys: Arc<[String]> = self.tags.iter().map(|t| t.key.clone()).collect();
        let has_key = move || {
            let keys = Arc::clone(&keys);
            move |strings: &osmflow::BlockStrings<'_>| keys.iter().any(|k| strings.has(k))
        };
        file.nodes_where(has_key())
            .ways_where(has_key())
            .relations_where(has_key())
    }

    /// For a read that only `--id` chooses objects of, in a file sorted by type then ID: what tells it to stop once
    /// it is past the highest ID wanted of each type.
    pub fn id_stop(&self, sorted: bool) -> Option<IdStop> {
        if !sorted || self.ids.is_empty() || !self.tags.is_empty() {
            return None;
        }
        let mut left = [None; 3];
        for kind in [Kind::Node, Kind::Way, Kind::Relation] {
            if self.wants(kind) {
                left[kind.index()] = self
                    .ids
                    .iter()
                    .filter(|s| s.kind.is_none_or(|k| k == kind))
                    .map(|s| s.id)
                    .max();
            }
        }
        Some(IdStop { left })
    }

    /// A test of single objects against the flags.
    pub fn matcher(&self) -> Matcher {
        let mut any = Vec::new();
        let mut typed: [Vec<i64>; 3] = Default::default();
        for spec in &self.ids {
            match spec.kind {
                Some(kind) => typed[kind.index()].push(spec.id),
                None => any.push(spec.id),
            }
        }
        for ids in typed.iter_mut().chain([&mut any]) {
            ids.sort_unstable();
        }
        Matcher {
            ids: !self.ids.is_empty(),
            any,
            typed,
            tags: self.tags.clone(),
        }
    }
}

/// Tells a read by ID in a sorted file when it has passed every ID it wants, from [`Filter::id_stop`].
#[derive(Debug)]
pub struct IdStop {
    /// The highest ID still to reach of each type; `None` for a type that is done or not wanted.
    left: [Option<i64>; 3],
}

impl IdStop {
    /// Notes that a block's objects of `kind` end at `last`; whether every wanted type is now done. Types not read
    /// may be given too, and change nothing.
    pub fn passed(&mut self, kind: Kind, last: i64) -> bool {
        let slot = &mut self.left[kind.index()];
        if slot.is_some_and(|max| last >= max) {
            *slot = None;
        }
        self.left.iter().all(Option::is_none)
    }

    /// Notes the last object of each type in `block`; whether the read is done.
    pub fn block<T: TagMode>(&mut self, block: &Block<T>) -> bool {
        let mut done = false;
        if let Some(n) = block.nodes().last() {
            done = self.passed(Kind::Node, n.id());
        }
        if let Some(w) = block.ways().last() {
            done = self.passed(Kind::Way, w.id());
        }
        if let Some(r) = block.relations().last() {
            done = self.passed(Kind::Relation, r.id());
        }
        done
    }
}

/// Tests objects against the `--id` and `--tag` flags.
#[derive(Debug)]
pub struct Matcher {
    ids: bool,
    /// IDs of any type, sorted, and IDs of each type, by [`Kind::index`]: a few, searched in place.
    any: Vec<i64>,
    typed: [Vec<i64>; 3],
    tags: Vec<TagSpec>,
}

impl Matcher {
    /// Whether an object of `kind` with `id` and `tags` is chosen. Its type is checked by [`Filter::select`].
    pub fn matches<'a>(
        &self,
        kind: Kind,
        id: i64,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> bool {
        if self.ids
            && self.any.binary_search(&id).is_err()
            && self.typed[kind.index()].binary_search(&id).is_err()
        {
            return false;
        }
        if self.tags.is_empty() {
            return true;
        }
        for (k, v) in tags {
            if self
                .tags
                .iter()
                .any(|t| t.key == k && (t.values.is_empty() || t.values.iter().any(|tv| tv == v)))
            {
                return true;
            }
        }
        false
    }
}

/// How many threads decode the file.
#[derive(Debug, clap::Args)]
pub struct Threads {
    /// Threads to decode on; by default, two fewer than the cores.
    ///
    /// With 1, blocks are decoded one after another on one thread.
    #[arg(short = 'j', long, value_name = "N")]
    threads: Option<NonZeroUsize>,
}

impl Threads {
    fn sequential(&self) -> bool {
        self.threads.is_some_and(|n| n.get() == 1)
    }

    /// The number of threads asked for, if any.
    pub fn count(&self) -> Option<usize> {
        self.threads.map(NonZeroUsize::get)
    }

    /// `file`, decoding on a pool of the chosen size.
    pub fn apply(&self, file: PbfReader) -> Result<PbfReader> {
        // A pool of one thread too, for the reads that always use the pool, such as loading nodes.
        match self.threads {
            Some(n) => {
                let pool = ThreadPoolBuilder::new().num_threads(n.get()).build()?;
                Ok(file.with_pool(Arc::new(pool)))
            }
            None => Ok(file),
        }
    }

    /// Calls `f` with each block of `file` in file order, until it breaks: decoded on this thread when sequential,
    /// otherwise decoded ahead on the pool.
    pub fn ordered<T: TagMode>(
        &self,
        file: &PbfReader,
        select: Select<T>,
        mut f: impl FnMut(Block<T>) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        let blocks: Box<dyn Iterator<Item = Result<Block<T>, PbfError>>> = if self.sequential() {
            Box::new(file.blocks(select)?)
        } else {
            Box::new(file.par_ordered(select)?)
        };
        for block in blocks {
            if f(block?)?.is_break() {
                break;
            }
        }
        Ok(())
    }

    /// Calls `f` with every block of `file`, in no particular order when parallel.
    pub fn unordered<T: TagMode>(
        &self,
        file: &PbfReader,
        select: Select<T>,
        f: impl Fn(Block<T>) + Sync,
    ) -> Result<()> {
        if self.sequential() {
            for block in file.blocks(select)? {
                f(block?);
            }
        } else {
            file.par_unordered(select, |block| {
                f(block);
                Ok(())
            })?;
        }
        Ok(())
    }
}

/// The flags of osmChange files and replication streams: which diffs to read, where to keep them, and which changes
/// to take.
#[derive(Debug, Default, clap::Args)]
#[command(next_help_heading = "osmChange and replication")]
pub struct ChangeArgs {
    /// Diffs to read: `N`, `A..B`, `A..` or `latest`.
    ///
    /// `A..B` includes both ends, and `A..` runs to the latest state. Without it or --since, `dump` of a stream
    /// reads the latest diff, and `stats` shows the stream's latest state.
    #[arg(long, value_name = "SEQ", value_parser = parse_seqs)]
    pub seq: Option<Seqs>,
    /// Diffs after a PBF file, a state file or a time.
    ///
    /// After a PBF file, by the replication sequence in its header when its base URL is this stream's, and otherwise
    /// by its replication timestamp. After a state file of this stream. Or the changes made after a time, as
    /// `2026-11-09T21:00:00Z` or `2026-11-09`. From a PBF file's timestamp, the diffs start 6 minutes before it, as a
    /// change can show up in a diff that much after it is made, so the first ones may hold changes the file has.
    #[arg(long, value_name = "WHEN", value_parser = parse_since, conflicts_with = "seq")]
    pub since: Option<Since>,
    /// Where to keep a server's downloaded files.
    ///
    /// Each file is downloaded into it once, and read from it after. Reading a server's diffs needs it; its latest
    /// state does not.
    #[arg(long, value_name = "DIR")]
    pub cache: Option<PathBuf>,
    /// Only changes in these sections, by commas.
    #[arg(long = "action", value_name = "ACTION", value_delimiter = ',', value_parser = names::<Action>())]
    actions: Vec<Action>,
    /// Only changes in these changesets, by commas.
    ///
    /// Geofabrik's public diffs have no changesets.
    #[arg(long = "changeset", value_name = "ID", value_delimiter = ',')]
    changesets: Vec<i64>,
    /// Only changes by these user names, by commas.
    ///
    /// A name made of digits is a name too; --uid takes user IDs. Geofabrik's public diffs have no users.
    #[arg(long = "user", value_name = "NAME", value_delimiter = ',')]
    users: Vec<String>,
    /// Only changes by these user IDs, by commas.
    ///
    /// A change by any user --user or --uid names is chosen. Geofabrik's public diffs have no users.
    #[arg(long = "uid", value_name = "UID", value_delimiter = ',')]
    uids: Vec<i32>,
}

impl ChangeArgs {
    /// The first flag given that only replication streams take, for an error about a file.
    pub fn stream_flag(&self) -> Option<&'static str> {
        first_given(&[
            (self.seq.is_some(), "--seq"),
            (self.since.is_some(), "--since"),
            (self.cache.is_some(), "--cache"),
        ])
    }

    /// The first flag given that only osmChange input takes, for an error about a PBF file.
    pub fn given(&self) -> Option<&'static str> {
        self.stream_flag().or_else(|| {
            first_given(&[
                (!self.actions.is_empty(), "--action"),
                (!self.changesets.is_empty(), "--changeset"),
                (!self.users.is_empty(), "--user"),
                (!self.uids.is_empty(), "--uid"),
            ])
        })
    }

    /// Fails if a flag that only osmChange input takes is given for the PBF file `path`.
    pub fn reject_pbf(&self, path: &Path) -> Result<()> {
        match self.given() {
            Some(flag) => bail!(
                "{flag} is for osmChange files and replication streams, and {} is a PBF file",
                path.display()
            ),
            None => Ok(()),
        }
    }

    /// Whether `--seq` or `--since` chooses diffs.
    pub fn chooses(&self) -> bool {
        self.seq.is_some() || self.since.is_some()
    }

    /// Whether `change` passes `--action`, `--changeset` and `--user`.
    pub fn matches(&self, change: &Change) -> bool {
        let meta = change.object.meta();
        (self.actions.is_empty() || self.actions.contains(&change.action))
            && (self.changesets.is_empty()
                || meta.changeset.is_some_and(|c| self.changesets.contains(&c)))
            && ((self.users.is_empty() && self.uids.is_empty())
                || meta.uid.is_some_and(|uid| self.uids.contains(&uid))
                || meta
                    .user
                    .as_deref()
                    .is_some_and(|name| self.users.iter().any(|u| u == name)))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use osmflow::osc::OscReader;

    use super::*;

    #[test]
    fn ids_and_tags_parse() {
        assert_eq!(
            parse_id("w15").unwrap(),
            IdSpec {
                kind: Some(Kind::Way),
                id: 15
            }
        );
        assert_eq!(parse_id("-3").unwrap(), IdSpec { kind: None, id: -3 });
        assert!(parse_id("x1").is_err() && parse_id("w").is_err());
        assert_eq!(parse_tag("highway").unwrap().values, Vec::<String>::new());
        assert_eq!(
            parse_tag("highway=primary,trunk").unwrap().values,
            ["primary", "trunk"]
        );
        parse_tag("=x").unwrap_err();
    }

    fn filter(ids: &[&str], tags: &[&str], types: &[Kind]) -> Filter {
        Filter {
            ids: ids.iter().map(|s| parse_id(s).unwrap()).collect(),
            types: types.to_vec(),
            tags: tags.iter().map(|s| parse_tag(s).unwrap()).collect(),
        }
    }

    #[test]
    fn typed_ids_choose_the_types_read() {
        let f = filter(&["w15", "r2"], &[], &[]);
        assert!(!f.wants(Kind::Node) && f.wants(Kind::Way) && f.wants(Kind::Relation));
        let f = filter(&["w15", "7"], &[], &[Kind::Node]);
        assert!(f.wants(Kind::Node) && !f.wants(Kind::Way));
    }

    #[test]
    fn id_reads_stop_past_the_highest_id() {
        let f = filter(&["w15", "w3", "r2"], &[], &[]);
        assert!(f.id_stop(false).is_none());
        assert!(filter(&["w1"], &["highway"], &[]).id_stop(true).is_none());
        let mut stop = f.id_stop(true).unwrap();
        assert!(!stop.passed(Kind::Node, 100));
        assert!(!stop.passed(Kind::Way, 14));
        assert!(!stop.passed(Kind::Way, 15));
        assert!(stop.passed(Kind::Relation, 7));
    }

    #[test]
    fn objects_match_ids_and_any_tag() {
        let m = filter(&["w15", "7"], &[], &[]).matcher();
        assert!(m.matches(Kind::Way, 15, [].into_iter()));
        assert!(!m.matches(Kind::Node, 15, [].into_iter()));
        assert!(m.matches(Kind::Relation, 7, [].into_iter()));
        let m = filter(&[], &["highway=primary,trunk", "building"], &[]).matcher();
        assert!(m.matches(Kind::Way, 1, [("highway", "trunk")].into_iter()));
        assert!(m.matches(Kind::Way, 1, [("building", "yes")].into_iter()));
        assert!(!m.matches(Kind::Way, 1, [("highway", "path")].into_iter()));
        assert!(!m.matches(Kind::Way, 1, [].into_iter()));
    }

    #[test]
    fn changes_match_action_changeset_and_user() {
        let xml = r#"<osmChange version="0.6">
            <create><node id="1" lat="1" lon="1" changeset="10" uid="5" user="ann"/></create>
            <delete><way id="2" changeset="11" uid="6" user="bob"/></delete>
            <modify><relation id="3"/></modify>
        </osmChange>"#;
        let changes: Vec<Change> = OscReader::new(Cursor::new(xml))
            .collect::<Result<_, _>>()
            .unwrap();
        let chosen = |args: ChangeArgs| -> Vec<i64> {
            changes
                .iter()
                .filter(|c| args.matches(c))
                .map(|c| c.object.id())
                .collect()
        };
        assert_eq!(chosen(ChangeArgs::default()), [1, 2, 3]);
        let actions = vec![Action::Delete, Action::Modify];
        assert_eq!(
            chosen(ChangeArgs {
                actions,
                ..ChangeArgs::default()
            }),
            [2, 3]
        );
        let changesets = vec![10];
        assert_eq!(
            chosen(ChangeArgs {
                changesets,
                ..ChangeArgs::default()
            }),
            [1]
        );
        // A name of digits is a name, not an ID.
        let (users, uids) = (vec!["bob".to_owned(), "6".to_owned()], vec![5]);
        assert_eq!(
            chosen(ChangeArgs {
                users,
                uids,
                ..ChangeArgs::default()
            }),
            [1, 2]
        );
        let users = vec!["5".to_owned()];
        assert_eq!(
            chosen(ChangeArgs {
                users,
                ..ChangeArgs::default()
            }),
            [] as [i64; 0]
        );
        assert_eq!(ChangeArgs::default().given(), None);
        let users = vec!["x".to_owned()];
        assert_eq!(
            ChangeArgs {
                users,
                ..ChangeArgs::default()
            }
            .given(),
            Some("--user")
        );
    }
}
