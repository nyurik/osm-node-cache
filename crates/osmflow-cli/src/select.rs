//! What a command reads: which objects (by type, ID and tag), and on how many threads.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use osmflow::rayon::ThreadPoolBuilder;
use osmflow::{Block, PbfError, PbfReader, Select, TagMode, Tags};

/// An object type, as the flags name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum Kind {
    Node,
    Way,
    Relation,
}

impl Kind {
    fn index(self) -> usize {
        match self {
            Self::Node => 0,
            Self::Way => 1,
            Self::Relation => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Way => "way",
            Self::Relation => "relation",
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
    /// Only objects with these IDs: `12` of any type, or `n12`, `w12` or `r12` of one. Repeat the flag, or separate
    /// IDs with commas.
    #[arg(long = "id", value_name = "ID", value_delimiter = ',', value_parser = parse_id)]
    ids: Vec<IdSpec>,
    /// Only objects of these types, separated by commas.
    #[arg(short = 't', long = "type", value_name = "TYPE", value_delimiter = ',')]
    types: Vec<Kind>,
    /// Only objects with this tag: `KEY` with any value, or `KEY=VALUE` or `KEY=V1,V2` with one of the values. Given
    /// more than once, an object needs any one of them. Blocks without the key are skipped unread.
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
        let mut any = HashSet::new();
        let mut typed = HashSet::new();
        for spec in &self.ids {
            match spec.kind {
                Some(kind) => typed.insert((kind, spec.id)),
                None => any.insert(spec.id),
            };
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
    any: HashSet<i64>,
    typed: HashSet<(Kind, i64)>,
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
        if self.ids && !self.any.contains(&id) && !self.typed.contains(&(kind, id)) {
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
    /// Threads to decode on. With 1, blocks are decoded one after another on one thread; by default a pool shared by
    /// every read decodes, with two threads fewer than the cores.
    #[arg(short = 'j', long, value_name = "N")]
    threads: Option<NonZeroUsize>,
}

impl Threads {
    fn sequential(&self) -> bool {
        self.threads.is_some_and(|n| n.get() == 1)
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
    pub fn unordered(
        &self,
        file: &PbfReader,
        select: Select<Tags>,
        f: impl Fn(Block) + Sync,
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

#[cfg(test)]
mod tests {
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
}
