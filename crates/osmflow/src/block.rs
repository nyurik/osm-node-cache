//! A decoded block of objects, held as columns, and the views of its nodes, ways and relations.

use std::borrow::Cow;
use std::iter::FusedIterator;
use std::marker::PhantomData;
use std::ops::Range;
use std::{ptr, result, str};

use buffa::MessageView as _;

use crate::error::{BlobPosition, PbfError, PbfFormatError, Result};
use crate::filter::Filters;
use crate::generated::raw::osmflow_raw::{
    DenseInfoView, DenseNodeLocationsView, DenseNodesView, PrimitiveBlockView,
};
use crate::model::{Member, MemberType};
use crate::packed::{self, Value};
use crate::time::Timestamp;

type FResult<T> = result::Result<T, PbfFormatError>;

/// Whether a read decodes tags, as a type: [`Tags`] or [`NoTags`]. Code that reads tags from a read without them
/// does not compile:
///
/// ```compile_fail
/// fn tagless(path: &std::path::Path) -> Result<(), osmflow::PbfError> {
///     let file = osmflow::PbfReader::open(path)?;
///     for block in file.blocks(osmflow::Select::node_locations())? {
///         for node in block?.nodes() {
///             let _ = node.tags(); // no tags method: the read has no tags
///         }
///     }
///     Ok(())
/// }
/// ```
pub trait TagMode: sealed::Sealed + Copy + Send + Sync + 'static {
    /// Whether tags are decoded.
    const TAGS: bool;
}

/// Tags are decoded: the tag methods of [`Node`], [`Way`], [`Relation`] and [`Block`] are available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tags;

/// Tags are not decoded: faster when they are not needed, and their methods are not available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoTags;

impl TagMode for Tags {
    const TAGS: bool = true;
}

impl TagMode for NoTags {
    const TAGS: bool = false;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Tags {}
    impl Sealed for super::NoTags {}
}

/// Which objects and fields to decode. Anything not selected is skipped without being decoded. Whether tags are
/// decoded is the type `T`: [`Select::all`] decodes them, [`Select::without_tags`] and [`Select::node_locations`] do
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent switches")]
pub struct Select<T: TagMode = Tags> {
    /// Nodes, plain and dense. Without them, blocks of only nodes are skipped unread.
    nodes: bool,
    /// Ways and their node refs.
    ways: bool,
    /// Relations and their members.
    relations: bool,
    /// Version, timestamp, changeset, user and visibility of the selected objects.
    metadata: bool,
    tags: PhantomData<T>,
}

impl Select<Tags> {
    /// Every object and field.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            nodes: true,
            ways: true,
            relations: true,
            metadata: true,
            tags: PhantomData,
        }
    }
}

impl Select<NoTags> {
    /// Node IDs and locations only.
    #[must_use]
    pub const fn node_locations() -> Self {
        Self {
            nodes: true,
            ways: false,
            relations: false,
            metadata: false,
            tags: PhantomData,
        }
    }
}

impl<T: TagMode> Select<T> {
    /// The same selection, without nodes, plain and dense: blocks of only nodes are then skipped unread.
    #[must_use]
    pub const fn without_nodes(self) -> Self {
        Self {
            nodes: false,
            ..self
        }
    }

    /// The same selection, without ways and their node refs.
    #[must_use]
    pub const fn without_ways(self) -> Self {
        Self {
            ways: false,
            ..self
        }
    }

    /// The same selection, without relations and their members.
    #[must_use]
    pub const fn without_relations(self) -> Self {
        Self {
            relations: false,
            ..self
        }
    }

    /// The same selection, without metadata: the version, timestamp, changeset, user and visibility of objects.
    #[must_use]
    pub const fn without_metadata(self) -> Self {
        Self {
            metadata: false,
            ..self
        }
    }

    /// The same selection, without tags.
    #[must_use]
    pub const fn without_tags(self) -> Select<NoTags> {
        Select {
            nodes: self.nodes,
            ways: self.ways,
            relations: self.relations,
            metadata: self.metadata,
            tags: PhantomData,
        }
    }

    pub(crate) const fn fields(self) -> Fields {
        Fields {
            nodes: self.nodes,
            ways: self.ways,
            relations: self.relations,
            tags: T::TAGS,
            metadata: self.metadata,
        }
    }
}

/// What a read decodes, as decoding uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent switches")]
pub(crate) struct Fields {
    pub(crate) nodes: bool,
    pub(crate) ways: bool,
    pub(crate) relations: bool,
    pub(crate) tags: bool,
    pub(crate) metadata: bool,
}

impl Fields {
    fn strings(self) -> bool {
        self.tags || self.metadata || self.relations
    }
}

/// A column's length, stored in another column's `ends`. A block is at most 32 MiB, and each item of a column takes
/// at least one byte of it, so no column reaches `u32::MAX` items.
fn end(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// The range of item `i` in a column cut by `ends`; empty if there is no item `i` (the column was not selected).
fn range(ends: &[u32], i: usize) -> Range<usize> {
    let Some(&end) = ends.get(i) else {
        return 0..0;
    };
    let start = i.checked_sub(1).map_or(0, |prev| ends[prev]);
    start as usize..end as usize
}

/// A block's string table: checked as UTF-8 once, and copied out of the block, which can then be dropped.
#[derive(Debug, Default)]
struct Strings {
    text: String,
    ends: Vec<u32>,
}

impl Strings {
    /// Copies the strings into one buffer and checks it as UTF-8 once. The buffer being valid leaves one way for a
    /// string to be invalid: a character split between it and the next, so every end must be a character boundary.
    fn new(table: &[&[u8]]) -> FResult<Self> {
        let mut bytes = Vec::with_capacity(table.iter().map(|s| s.len()).sum());
        let mut ends = Vec::with_capacity(table.len());
        for s in table {
            bytes.extend_from_slice(s);
            ends.push(end(bytes.len()));
        }
        let bad = |at: usize| {
            PbfFormatError::InvalidUtf8(end(ends.partition_point(|&e| e as usize <= at)))
        };
        let text = String::from_utf8(bytes).map_err(|e| bad(e.utf8_error().valid_up_to()))?;
        if let Some(&e) = ends.iter().find(|&&e| !text.is_char_boundary(e as usize)) {
            return Err(bad(e as usize - 1));
        }
        Ok(Self { text, ends })
    }

    fn get(&self, index: u32) -> &str {
        &self.text[range(&self.ends, index as usize)]
    }
}

fn check_string(index: u32, strings: usize) -> FResult<()> {
    if (index as usize) < strings {
        Ok(())
    } else {
        Err(PbfFormatError::StringIndex(index))
    }
}

/// The occurrences of a message field, merged as protobuf merges them: decoding their concatenation. One occurrence,
/// the usual case, stays borrowed; none is `None`.
fn merged<'a>(occurrences: impl IntoIterator<Item = &'a [u8]>) -> Option<Cow<'a, [u8]>> {
    occurrences
        .into_iter()
        .fold(None, |acc, next| Some(merge(acc, next)))
}

/// Appends an occurrence of a message field to those before it; see [`merged`]. The first occurrence is borrowed,
/// the second copies it once, and later ones are appended to that copy, so merging takes linear time.
fn merge<'a>(acc: Option<Cow<'a, [u8]>>, next: &'a [u8]) -> Cow<'a, [u8]> {
    match acc {
        None => Cow::Borrowed(next),
        Some(prev) => {
            let mut all = prev.into_owned();
            all.extend_from_slice(next);
            Cow::Owned(all)
        }
    }
}

/// The tags of a column of objects, as string indexes: object `i` has `keys[r]` and `vals[r]`, `r = range(ends, i)`.
#[derive(Debug, Default)]
struct TagColumns {
    ends: Vec<u32>,
    keys: Vec<u32>,
    vals: Vec<u32>,
}

impl TagColumns {
    /// Ends one object's tags, once its keys and values from `first` on are appended: they must pair up, and name
    /// strings of the block's string table.
    fn end_object(&mut self, first: usize, strings: usize) -> FResult<()> {
        if self.keys.len() != self.vals.len() {
            return Err(PbfFormatError::LengthMismatch("tag keys and values"));
        }
        for (&k, &v) in self.keys[first..].iter().zip(&self.vals[first..]) {
            check_string(k, strings)?;
            check_string(v, strings)?;
        }
        self.ends.push(end(self.keys.len()));
        Ok(())
    }

    /// Appends the tags of `count` dense nodes: for each node, key and value indexes, then a 0. An empty array means
    /// no node has tags.
    fn push_dense(&mut self, keys_vals: &[i32], count: usize, strings: usize) -> FResult<()> {
        if keys_vals.is_empty() {
            self.ends
                .resize(self.ends.len() + count, end(self.keys.len()));
            return Ok(());
        }
        let first = self.ends.len();
        let mut key = None;
        for &v in keys_vals {
            let v = u32::try_from(v).map_err(|_| PbfFormatError::Packed("dense node tags"))?;
            match key.take() {
                None if v == 0 => self.ends.push(end(self.keys.len())),
                None => key = Some(v),
                Some(k) => {
                    check_string(k, strings)?;
                    check_string(v, strings)?;
                    self.keys.push(k);
                    self.vals.push(v);
                }
            }
        }
        if key.is_some() {
            return Err(PbfFormatError::DenseTags("a key has no value"));
        }
        if self.ends.len() - first != count {
            return Err(PbfFormatError::DenseTags(
                "the number of tag lists is not the number of nodes",
            ));
        }
        Ok(())
    }
}

/// [`Info`], with the user as a string index.
#[derive(Debug, Clone, Copy)]
struct RawInfo {
    version: i32,
    uid: i32,
    user: u32,
    visible: bool,
    timestamp_ms: i64,
    changeset: i64,
}

/// The metadata fields a file leaves out.
const NO_INFO: RawInfo = RawInfo {
    version: -1,
    uid: 0,
    user: 0,
    visible: true,
    timestamp_ms: 0,
    changeset: 0,
};

/// What decoding an object needs besides the object.
struct Ctx {
    select: Fields,
    /// The number of strings in the block's string table.
    strings: usize,
    date_granularity: i64,
}

impl Ctx {
    /// Decodes a plain object's `Info` message.
    fn info(&self, msg: &[u8]) -> FResult<RawInfo> {
        let bad = || PbfFormatError::Message("info");
        let mut info = NO_INFO;
        packed::fields(msg, "info", |field, value| {
            let Value::Varint(v) = value else {
                return if field <= 6 { Err(bad()) } else { Ok(()) };
            };
            // int32 and int64 fields are written as the varint of their (sign-extended) two's complement.
            match field {
                1 => info.version = i32::try_from(v.cast_signed()).map_err(|_| bad())?,
                2 => info.timestamp_ms = v.cast_signed().wrapping_mul(self.date_granularity),
                3 => info.changeset = v.cast_signed(),
                4 => info.uid = i32::try_from(v.cast_signed()).map_err(|_| bad())?,
                5 => {
                    info.user = u32::try_from(v).map_err(|_| bad())?;
                    check_string(info.user, self.strings)?;
                }
                6 => info.visible = v != 0,
                _ => {}
            }
            Ok(())
        })?;
        Ok(info)
    }

    /// Decodes the `DenseInfo` message of `count` dense nodes, each array straight into its field of the nodes'
    /// metadata. Each array is either complete or absent; an absent one leaves its field at the default.
    fn dense_info(&self, info: &[u8], count: usize, out: &mut Vec<Option<RawInfo>>) -> FResult<()> {
        let v = DenseInfoView::decode_view(info).map_err(PbfFormatError::protobuf("dense info"))?;
        let first = out.len();
        out.resize(first + count, Some(NO_INFO));
        let infos = &mut out[first..];
        let gran = self.date_granularity;
        let strings = self.strings;
        // Delta-coded arrays carry a running sum.
        fill(infos, &v.version, "dense versions", |i, v| {
            i.version = v;
            Some(())
        })?;
        let mut sum = 0i64;
        fill(infos, &v.timestamp, "dense timestamps", |i, v| {
            sum = sum.wrapping_add(v);
            i.timestamp_ms = sum.wrapping_mul(gran);
            Some(())
        })?;
        sum = 0;
        fill(infos, &v.changeset, "dense changesets", |i, v| {
            sum = sum.wrapping_add(v);
            i.changeset = sum;
            Some(())
        })?;
        sum = 0;
        fill(infos, &v.uid, "dense user ids", |i, v| {
            sum = sum.wrapping_add(i64::from(v));
            i.uid = i32::try_from(sum).ok()?;
            Some(())
        })?;
        sum = 0;
        fill(infos, &v.user_sid, "dense user names", |i, v| {
            sum = sum.wrapping_add(i64::from(v));
            i.user = u32::try_from(sum)
                .ok()
                .filter(|&s| (s as usize) < strings)?;
            Some(())
        })?;
        fill(infos, &v.visible, "dense visibility", |i, v| {
            i.visible = v;
            Some(())
        })
    }
}

/// Sets one field of every node's metadata from a packed array, which must have one value per node or none. `set`
/// rejects a value by returning `None`.
fn fill<T: Copy>(
    infos: &mut [Option<RawInfo>],
    values: &[T],
    what: &'static str,
    mut set: impl FnMut(&mut RawInfo, T) -> Option<()>,
) -> FResult<()> {
    if values.is_empty() {
        return Ok(());
    }
    if values.len() != infos.len() {
        return Err(PbfFormatError::LengthMismatch(
            "dense nodes and their metadata",
        ));
    }
    for (slot, &v) in infos.iter_mut().flatten().zip(values) {
        set(slot, v).ok_or(PbfFormatError::Packed(what))?;
    }
    Ok(())
}

/// The columns every object type has.
#[derive(Debug, Default)]
struct Objects {
    ids: Vec<i64>,
    /// Empty unless tags are selected.
    tags: TagColumns,
    /// Empty unless metadata is selected.
    infos: Vec<Option<RawInfo>>,
}

impl Objects {
    /// Decodes a plain (not dense) object's message: its ID (zigzag-coded for nodes), tags and metadata here, and
    /// every other field with `own`.
    fn push(
        &mut self,
        msg: &[u8],
        what: &'static str,
        zigzag_id: bool,
        ctx: &Ctx,
        mut own: impl FnMut(u32, Value<'_>) -> FResult<()>,
    ) -> FResult<()> {
        let (mut id, mut info) = (None, None);
        let first = self.tags.keys.len();
        let (keys, vals) = (&mut self.tags.keys, &mut self.tags.vals);
        packed::fields(msg, what, |field, value| match (field, value) {
            (1, Value::Varint(v)) => {
                id = Some(if zigzag_id {
                    packed::zigzag(v)
                } else {
                    v.cast_signed()
                });
                Ok(())
            }
            (2, v) if ctx.select.tags => {
                packed::values(v, "tag keys", |k| push_index(keys, k, "tag keys"))
            }
            (3, v) if ctx.select.tags => {
                packed::values(v, "tag values", |k| push_index(vals, k, "tag values"))
            }
            (4, Value::Bytes(b)) => {
                if ctx.select.metadata {
                    info = Some(merge(info.take(), b));
                }
                Ok(())
            }
            (1 | 4, _) => Err(PbfFormatError::Message(what)),
            (field, v) => own(field, v),
        })?;
        self.ids.push(id.ok_or(PbfFormatError::MissingId)?);
        if ctx.select.tags {
            self.tags.end_object(first, ctx.strings)?;
        }
        if ctx.select.metadata {
            self.infos.push(info.map(|i| ctx.info(&i)).transpose()?);
        }
        Ok(())
    }
}

/// Appends a string index; a value beyond `u32` (a negative `int32` among them) is an error.
fn push_index(out: &mut Vec<u32>, v: u64, what: &'static str) -> FResult<()> {
    out.push(u32::try_from(v).map_err(|_| PbfFormatError::Packed(what))?);
    Ok(())
}

#[derive(Debug, Default)]
struct Nodes {
    objects: Objects,
    lats: Vec<i64>,
    lons: Vec<i64>,
}

impl Nodes {
    fn push(&mut self, msg: &[u8], ctx: &Ctx) -> FResult<()> {
        let (mut lat, mut lon) = (None, None);
        self.objects.push(msg, "node", true, ctx, |field, value| {
            match (field, value) {
                (8, Value::Varint(v)) => lat = Some(packed::zigzag(v)),
                (9, Value::Varint(v)) => lon = Some(packed::zigzag(v)),
                (8 | 9, _) => return Err(PbfFormatError::Message("node")),
                _ => {}
            }
            Ok(())
        })?;
        self.lats
            .push(lat.ok_or(PbfFormatError::NodeLocation("latitude"))?);
        self.lons
            .push(lon.ok_or(PbfFormatError::NodeLocation("longitude"))?);
        Ok(())
    }

    /// Decodes a group's dense nodes: all their fields, or only their locations when no tags or metadata are
    /// selected, which skips the rest unread.
    ///
    /// buffa's views decode each packed array into a `Vec`, which is read again here.
    fn push_dense(&mut self, dense: &[u8], ctx: &Ctx) -> FResult<()> {
        let bad = PbfFormatError::protobuf("dense nodes");
        if !ctx.select.tags && !ctx.select.metadata {
            let d = DenseNodeLocationsView::decode_view(dense).map_err(bad)?;
            self.push_locations(&d.id, &d.lat, &d.lon)?;
            return Ok(());
        }
        let d = DenseNodesView::decode_view(dense).map_err(bad)?;
        let count = self.push_locations(&d.id, &d.lat, &d.lon)?;
        let o = &mut self.objects;
        if ctx.select.tags {
            o.tags.push_dense(&d.keys_vals, count, ctx.strings)?;
        }
        if ctx.select.metadata {
            match merged(d.denseinfo.iter().copied()) {
                Some(info) => ctx.dense_info(&info, count, &mut o.infos)?,
                None => o.infos.resize(o.infos.len() + count, None),
            }
        }
        Ok(())
    }
}

impl Nodes {
    /// Appends dense nodes' IDs and locations, and returns how many there are.
    fn push_locations(&mut self, ids: &[i64], lats: &[i64], lons: &[i64]) -> FResult<usize> {
        let count = ids.len();
        if lats.len() != count || lons.len() != count {
            return Err(PbfFormatError::LengthMismatch(
                "dense node ids, latitudes and longitudes",
            ));
        }
        delta_sum(ids, &mut self.objects.ids);
        delta_sum(lats, &mut self.lats);
        delta_sum(lons, &mut self.lons);
        Ok(count)
    }
}

/// Appends the running sums of delta-coded values.
fn delta_sum(deltas: &[i64], out: &mut Vec<i64>) {
    let mut sum = 0i64;
    out.extend(deltas.iter().map(|&d| {
        sum = sum.wrapping_add(d);
        sum
    }));
}

#[derive(Debug, Default)]
struct Ways {
    objects: Objects,
    /// Each way's refs as the file packs them (zigzag varints of the deltas), checked, and decoded when iterated; and
    /// the running count of refs at each way's end.
    refs: Vec<u8>,
    ref_ends: Vec<u32>,
    ref_counts: Vec<u32>,
    /// Locations on ways: way `i` has none if `range(loc_ends, i)` is empty, and one per ref otherwise.
    loc_ends: Vec<u32>,
    lats: Vec<i64>,
    lons: Vec<i64>,
}

impl Ways {
    fn push(&mut self, msg: &[u8], ctx: &Ctx) -> FResult<()> {
        let (first, loc) = (self.refs.len(), self.lats.len());
        let (refs, lats, lons) = (&mut self.refs, &mut self.lats, &mut self.lons);
        let (mut lat, mut lon) = (0i64, 0i64);
        self.objects
            .push(msg, "way", false, ctx, |field, value| match field {
                // Kept packed: split arrays join as they are, and an unpacked value is packed again.
                8 => match value {
                    Value::Bytes(b) => {
                        refs.extend_from_slice(b);
                        Ok(())
                    }
                    Value::Varint(v) => {
                        packed::push_varint(refs, v);
                        Ok(())
                    }
                    Value::Fixed => Err(PbfFormatError::Packed("way refs")),
                },
                9 => packed::values(value, "way latitudes", |v| {
                    lat = lat.wrapping_add(packed::zigzag(v));
                    lats.push(lat);
                    Ok(())
                }),
                10 => packed::values(value, "way longitudes", |v| {
                    lon = lon.wrapping_add(packed::zigzag(v));
                    lons.push(lon);
                    Ok(())
                }),
                _ => Ok(()),
            })?;
        self.ref_ends.push(end(self.refs.len()));
        let n = packed::count(&self.refs[first..]).ok_or(PbfFormatError::Packed("way refs"))?;
        let counted = self.ref_counts.last().map_or(0, |&c| c as usize);
        self.ref_counts.push(end(counted + n));
        let (n_lat, n_lon) = (self.lats.len() - loc, self.lons.len() - loc);
        if (n_lat, n_lon) != (0, 0) && (n_lat, n_lon) != (n, n) {
            return Err(PbfFormatError::LengthMismatch(
                "way refs and their locations",
            ));
        }
        self.loc_ends.push(end(self.lats.len()));
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Relations {
    objects: Objects,
    member_ends: Vec<u32>,
    member_ids: Vec<i64>,
    member_types: Vec<MemberType>,
    member_roles: Vec<u32>,
}

impl Relations {
    fn push(&mut self, msg: &[u8], ctx: &Ctx) -> FResult<()> {
        let first = self.member_ids.len();
        let (ids, roles, types) = (
            &mut self.member_ids,
            &mut self.member_roles,
            &mut self.member_types,
        );
        let mut id = 0i64;
        self.objects
            .push(msg, "relation", false, ctx, |field, value| match field {
                8 => packed::values(value, "member roles", |v| {
                    push_index(roles, v, "member roles")
                }),
                9 => packed::values(value, "member ids", |v| {
                    id = id.wrapping_add(packed::zigzag(v));
                    ids.push(id);
                    Ok(())
                }),
                10 => packed::values(value, "member types", |t| {
                    types.push(match t {
                        0 => MemberType::Node,
                        1 => MemberType::Way,
                        2 => MemberType::Relation,
                        _ => return Err(PbfFormatError::MemberType(t)),
                    });
                    Ok(())
                }),
                _ => Ok(()),
            })?;
        let len = self.member_ids.len();
        if self.member_roles.len() != len || self.member_types.len() != len {
            return Err(PbfFormatError::LengthMismatch(
                "member ids, roles and types",
            ));
        }
        for &role in &self.member_roles[first..] {
            check_string(role, ctx.strings)?;
        }
        self.member_ends.push(end(len));
        Ok(())
    }
}

/// A block's granularity, which must be positive.
fn positive(value: i32, what: &'static str) -> FResult<i64> {
    if value > 0 {
        Ok(i64::from(value))
    } else {
        Err(PbfFormatError::Granularity { what, value })
    }
}

#[derive(Debug)]
struct Columns {
    strings: Strings,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
    nodes: Nodes,
    ways: Ways,
    relations: Relations,
}

impl Columns {
    /// The block's columns, and its layout without the blob's sizes, which the caller knows.
    fn decode(
        data: &[u8],
        select: Fields,
        filters: Option<&Filters>,
    ) -> FResult<(Self, BlockLayout)> {
        let view =
            PrimitiveBlockView::decode_view(data).map_err(PbfFormatError::protobuf("block"))?;
        let select = filters.map_or(select, |f| f.narrow(select, &view.stringtable.s));
        let strings = if select.strings() {
            Strings::new(&view.stringtable.s)?
        } else {
            Strings::default()
        };
        let ctx = Ctx {
            select,
            strings: strings.ends.len(),
            date_granularity: positive(view.date_granularity.unwrap_or(1000), "date granularity")?,
        };
        let mut groups = Vec::with_capacity(view.primitivegroup.len());
        let mut c = Self {
            strings,
            granularity: positive(view.granularity.unwrap_or(100), "granularity")?,
            lat_offset: view.lat_offset.unwrap_or(0),
            lon_offset: view.lon_offset.unwrap_or(0),
            nodes: Nodes::default(),
            ways: Ways::default(),
            relations: Relations::default(),
        };
        for group in &view.primitivegroup {
            let first = groups.len();
            if !group.nodes.is_empty() {
                groups.push(Group {
                    kind: GroupKind::Nodes,
                    objects: Some(group.nodes.len()),
                });
            }
            if select.nodes {
                for node in &group.nodes {
                    c.nodes.push(node, &ctx)?;
                }
                if let Some(dense) = merged(group.dense.iter().copied()) {
                    let before = c.nodes.objects.ids.len();
                    c.nodes.push_dense(&dense, &ctx)?;
                    groups.push(Group {
                        kind: GroupKind::DenseNodes,
                        objects: Some(c.nodes.objects.ids.len() - before),
                    });
                }
            } else if !group.dense.is_empty() {
                groups.push(Group {
                    kind: GroupKind::DenseNodes,
                    objects: None,
                });
            }
            if !group.ways.is_empty() {
                groups.push(Group {
                    kind: GroupKind::Ways,
                    objects: Some(group.ways.len()),
                });
            }
            if select.ways {
                for way in &group.ways {
                    c.ways.push(way, &ctx)?;
                }
            }
            if !group.relations.is_empty() {
                groups.push(Group {
                    kind: GroupKind::Relations,
                    objects: Some(group.relations.len()),
                });
            }
            if select.relations {
                for relation in &group.relations {
                    c.relations.push(relation, &ctx)?;
                }
            }
            if groups.len() == first {
                groups.push(Group {
                    kind: GroupKind::Empty,
                    objects: Some(0),
                });
            }
        }
        let layout = BlockLayout {
            blob_bytes: 0,
            raw_bytes: 0,
            zlib: false,
            strings: view.stringtable.s.len(),
            string_bytes: view.stringtable.s.iter().map(|s| s.len()).sum(),
            granularity: c.granularity,
            lat_offset: c.lat_offset,
            lon_offset: c.lon_offset,
            date_granularity: ctx.date_granularity,
            groups,
        };
        Ok((c, layout))
    }
}

/// A tag key's handle in one block's string table, from [`Block::tag_key`], to look up its value in that block's
/// objects with [`Node::tag`], [`Way::tag`] and [`Relation::tag`]: an integer compare per tag, with no string compares.
///
/// A handle belongs to the block it came from. Looking it up in an object of another block panics.
#[derive(Debug, Clone, Copy)]
pub struct TagKey<'b> {
    block: &'b Block<Tags>,
    index: u32,
    /// The string table has the key more than once, so tags are compared as strings.
    twice: bool,
}

/// A decoded block: its objects as columns, and its string table. It borrows nothing. `T` says whether its tags were
/// decoded.
#[derive(Debug)]
pub struct Block<T: TagMode = Tags> {
    position: BlobPosition,
    layout: BlockLayout,
    c: Columns,
    tags: PhantomData<T>,
}

/// How a block is stored in its file, from [`Block::layout`]: for inspecting files, not for reading their objects.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlockLayout {
    /// The size of the blob in the file, after its header: compressed, if it is.
    pub blob_bytes: usize,
    /// The size of the block uncompressed.
    pub raw_bytes: usize,
    /// Whether the blob is zlib-compressed; otherwise it is stored uncompressed.
    pub zlib: bool,
    /// The number of strings in the block's string table; the first is always empty.
    pub strings: usize,
    /// The bytes of all the strings together.
    pub string_bytes: usize,
    /// The block's coordinate unit in nanodegrees: 100 unless the writer chose another.
    pub granularity: i64,
    /// Added to every latitude, in nanodegrees.
    pub lat_offset: i64,
    /// Added to every longitude, in nanodegrees.
    pub lon_offset: i64,
    /// The block's timestamp unit in milliseconds: 1000 unless the writer chose another.
    pub date_granularity: i64,
    /// The block's groups, in file order. Writers put one object type in each group; most write one group per block.
    pub groups: Vec<Group>,
}

/// One group of a block's objects, from [`BlockLayout::groups`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    /// What the group holds.
    pub kind: GroupKind,
    /// How many objects it holds, or `None` for dense nodes when the read did not select nodes, which are counted only
    /// by decoding them.
    pub objects: Option<usize>,
}

/// What a block's group holds. A group with objects of several types, which the format does not allow but osmflow
/// reads, appears once per type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GroupKind {
    /// Nodes stored one message each, as few writers do.
    Nodes,
    /// Nodes stored as columns of deltas, as most writers do.
    DenseNodes,
    /// Ways.
    Ways,
    /// Relations.
    Relations,
    /// Nothing osmflow reads: no objects, or only changesets.
    Empty,
}

impl Block {
    /// Decodes a decompressed `PrimitiveBlock`, without the object types `filters` reject for its strings. The
    /// caller gives it the tag mode `select` was made with.
    pub(crate) fn decode(
        position: BlobPosition,
        blob_bytes: usize,
        zlib: bool,
        data: &[u8],
        select: Fields,
        filters: Option<&Filters>,
    ) -> Result<Self> {
        let (c, layout) = Columns::decode(data, select, filters)
            .map_err(|kind| PbfError::format(position, kind))?;
        Ok(Self {
            position,
            layout: BlockLayout {
                blob_bytes,
                raw_bytes: data.len(),
                zlib,
                ..layout
            },
            c,
            tags: PhantomData,
        })
    }

    /// The block, typed with the tag mode it was decoded with.
    pub(crate) fn with_mode<T: TagMode>(self) -> Block<T> {
        Block {
            position: self.position,
            layout: self.layout,
            c: self.c,
            tags: PhantomData,
        }
    }
}

impl<T: TagMode> Block<T> {
    /// Where the block is in its file.
    #[must_use]
    pub fn position(&self) -> BlobPosition {
        self.position
    }

    /// How the block is stored: its blob's sizes, string table, granularity and groups.
    #[must_use]
    pub fn layout(&self) -> &BlockLayout {
        &self.layout
    }

    /// The block's nodes, plain and dense, in file order.
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = Node<'_, T>> {
        (0..self.c.nodes.objects.ids.len()).map(move |i| Node { block: self, i })
    }

    /// Appends the block's nodes with IDs of 0 and up, in file order, as their IDs and locations in 1e-7 degrees: what
    /// [`Node::id`], [`Node::decimicro_lat`] and [`Node::decimicro_lon`] give, read straight from the columns.
    #[cfg(feature = "nodes")]
    pub(crate) fn push_node_locations(&self, out: &mut Vec<(u64, crate::Location)>) {
        let n = &self.c.nodes;
        let (lat0, lon0) = (self.c.lat_offset, self.c.lon_offset);
        out.reserve(n.objects.ids.len());
        out.extend(n.objects.ids.iter().zip(&n.lats).zip(&n.lons).filter_map(
            |((&id, &lat), &lon)| {
                let id = u64::try_from(id).ok()?;
                let loc = crate::Location::new(
                    decimicro(self.nano(lat, lat0)),
                    decimicro(self.nano(lon, lon0)),
                );
                Some((id, loc))
            },
        ));
    }

    /// The block's ways, in file order.
    pub fn ways(&self) -> impl ExactSizeIterator<Item = Way<'_, T>> {
        (0..self.c.ways.objects.ids.len()).map(move |i| Way { block: self, i })
    }

    /// The block's relations, in file order.
    pub fn relations(&self) -> impl ExactSizeIterator<Item = Relation<'_, T>> {
        (0..self.c.relations.objects.ids.len()).map(move |i| Relation { block: self, i })
    }
}

impl Block {
    /// The handle of tag key `key` in this block's string table, for [`Node::tag`], [`Way::tag`] and
    /// [`Relation::tag`]; one pass over the table. `None` if the block does not have it, so no object of the block
    /// has the tag.
    ///
    /// ```
    /// fn named_roads(block: &osmflow::Block) -> usize {
    ///     let (Some(highway), Some(name)) = (block.tag_key("highway"), block.tag_key("name")) else {
    ///         return 0; // no way of this block has both
    ///     };
    ///     block
    ///         .ways()
    ///         .filter(|way| way.tag(highway).is_some() && way.tag(name).is_some())
    ///         .count()
    /// }
    /// ```
    #[must_use]
    pub fn tag_key(&self, key: &str) -> Option<TagKey<'_>> {
        let strings = &self.c.strings;
        let mut found = None;
        for index in 0..strings.ends.len() {
            let index = end(index);
            if strings.get(index) == key {
                match &mut found {
                    None => {
                        found = Some(TagKey {
                            block: self,
                            index,
                            twice: false,
                        });
                    }
                    Some(k) => {
                        k.twice = true;
                        break;
                    }
                }
            }
        }
        found
    }

    fn tags<'a>(
        &'a self,
        objects: &'a Objects,
        i: usize,
    ) -> impl ExactSizeIterator<Item = (&'a str, &'a str)> + Clone + use<'a> {
        let (tags, strings) = (&objects.tags, &self.c.strings);
        let r = range(&tags.ends, i);
        tags.keys[r.clone()]
            .iter()
            .zip(&tags.vals[r])
            .map(move |(&k, &v)| (strings.get(k), strings.get(v)))
    }

    /// The value of tag `key` of object `i`, comparing string indexes; as strings if the table has the key twice.
    fn tag<'a>(&'a self, objects: &'a Objects, i: usize, key: TagKey<'_>) -> Option<&'a str> {
        assert!(
            ptr::eq(key.block, self),
            "a TagKey was looked up in an object of another block than the one it came from"
        );
        let tags = &objects.tags;
        let r = range(&tags.ends, i);
        let keys = &tags.keys[r.clone()];
        let j = if key.twice {
            let want = self.c.strings.get(key.index);
            keys.iter().position(|&k| self.c.strings.get(k) == want)?
        } else {
            keys.iter().position(|&k| k == key.index)?
        };
        Some(self.c.strings.get(tags.vals[r.start + j]))
    }
}

impl<T: TagMode> Block<T> {
    fn nano(&self, raw: i64, offset: i64) -> i64 {
        offset.wrapping_add(self.c.granularity.wrapping_mul(raw))
    }

    fn info(&self, objects: &Objects, i: usize) -> Option<Info<'_>> {
        let raw = (*objects.infos.get(i)?)?;
        Some(Info {
            version: raw.version,
            timestamp: Timestamp::from_millis(raw.timestamp_ms),
            changeset: raw.changeset,
            uid: raw.uid,
            user: self.c.strings.get(raw.user),
            visible: raw.visible,
        })
    }
}

/// Nanodegrees to 1e-7 degrees, truncated; a value outside `i32` is clamped to it.
fn decimicro(nano: i64) -> i32 {
    let d = nano / 100;
    i32::try_from(d).unwrap_or(if d < 0 { i32::MIN } else { i32::MAX })
}

/// An object's metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info<'a> {
    /// The object's version; -1 when the file does not give it.
    pub version: i32,
    /// When this version was made; [`Timestamp::EPOCH`] when the file does not give it.
    pub timestamp: Timestamp,
    /// The changeset that made this version; 0 when the file does not give it.
    pub changeset: i64,
    /// The ID of the user who made this version; 0 when the file does not give it.
    pub uid: i32,
    /// The user's name at the time of this version; empty when the file does not give it.
    pub user: &'a str,
    /// False for a deleted object, in a history file.
    pub visible: bool,
}

/// A view of one object of a [`Block`], with the accessors every object type has.
macro_rules! object_view {
    ($(#[$doc:meta])* $name:ident, $column:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name<'a, T: TagMode = Tags> {
            block: &'a Block<T>,
            i: usize,
        }

        impl<'a, T: TagMode> $name<'a, T> {
            /// The object's ID: positive in OSM data, negative for objects an editor created and has not uploaded.
            #[must_use]
            pub fn id(&self) -> i64 {
                self.block.c.$column.objects.ids[self.i]
            }

            /// The metadata, if it was selected and the file has it.
            #[must_use]
            pub fn info(&self) -> Option<Info<'a>> {
                self.block.info(&self.block.c.$column.objects, self.i)
            }
        }

        impl<'a> $name<'a> {
            /// The tags, as (key, value) pairs in file order.
            pub fn tags(&self) -> impl ExactSizeIterator<Item = (&'a str, &'a str)> + Clone + use<'a> {
                self.block.tags(&self.block.c.$column.objects, self.i)
            }

            /// The value of tag `key`, a handle from [`Block::tag_key`] of this object's block; none if the object does
            /// not have it. The first one, if the object has the key more than once.
            ///
            /// # Panics
            /// If `key` was found in another block.
            #[must_use]
            pub fn tag(&self, key: TagKey<'_>) -> Option<&'a str> {
                self.block.tag(&self.block.c.$column.objects, self.i, key)
            }
        }
    };
}

object_view!(
    /// A node of a [`Block`].
    Node,
    nodes
);
object_view!(
    /// A way of a [`Block`].
    Way,
    ways
);
object_view!(
    /// A relation of a [`Block`].
    Relation,
    relations
);

impl<T: TagMode> Node<'_, T> {
    /// Latitude in nanodegrees.
    #[must_use]
    pub fn lat_nano(&self) -> i64 {
        let b = self.block;
        b.nano(b.c.nodes.lats[self.i], b.c.lat_offset)
    }

    /// Longitude in nanodegrees.
    #[must_use]
    pub fn lon_nano(&self) -> i64 {
        let b = self.block;
        b.nano(b.c.nodes.lons[self.i], b.c.lon_offset)
    }

    /// Latitude in 1e-7 degrees, the unit OSM stores; truncated if the file is more precise.
    #[must_use]
    pub fn decimicro_lat(&self) -> i32 {
        decimicro(self.lat_nano())
    }

    /// Longitude in 1e-7 degrees.
    #[must_use]
    pub fn decimicro_lon(&self) -> i32 {
        decimicro(self.lon_nano())
    }
}

impl<'a, T: TagMode> Way<'a, T> {
    /// The IDs of the way's nodes, decoded as they are iterated.
    #[must_use]
    pub fn refs(&self) -> Refs<'a> {
        let w = &self.block.c.ways;
        let packed = &w.refs[range(&w.ref_ends, self.i)];
        Refs {
            packed,
            pos: 0,
            id: 0,
            left: range(&w.ref_counts, self.i).len(),
        }
    }

    /// The locations of the way's nodes, in nanodegrees (latitude, longitude), when the file has them
    /// (`LocationsOnWays`).
    #[must_use]
    pub fn locations(&self) -> Option<impl ExactSizeIterator<Item = (i64, i64)> + use<'a, T>> {
        let block = self.block;
        let w = &block.c.ways;
        let r = range(&w.loc_ends, self.i);
        if r.is_empty() {
            return None;
        }
        let (lat, lon) = (block.c.lat_offset, block.c.lon_offset);
        Some(
            w.lats[r.clone()]
                .iter()
                .zip(&w.lons[r])
                .map(move |(&y, &x)| (block.nano(y, lat), block.nano(x, lon))),
        )
    }
}

/// The IDs of a way's nodes, decoded from the file's packed form as they are iterated; from [`Way::refs`].
#[derive(Debug, Clone)]
pub struct Refs<'a> {
    packed: &'a [u8],
    pos: usize,
    id: i64,
    left: usize,
}

impl Iterator for Refs<'_> {
    type Item = i64;

    #[inline]
    fn next(&mut self) -> Option<i64> {
        // The varints were checked when the block was decoded.
        let v = packed::varint(self.packed, &mut self.pos)?;
        self.left -= 1;
        self.id = self.id.wrapping_add(packed::zigzag(v));
        Some(self.id)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl ExactSizeIterator for Refs<'_> {}

impl FusedIterator for Refs<'_> {}

impl<'a, T: TagMode> Relation<'a, T> {
    /// The relation's members, in order.
    pub fn members(&self) -> impl ExactSizeIterator<Item = Member<'a>> + Clone + use<'a, T> {
        let block = self.block;
        let r = &block.c.relations;
        range(&r.member_ends, self.i).map(move |m| Member {
            kind: r.member_types[m],
            id: r.member_ids[m],
            role: block.c.strings.get(r.member_roles[m]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad_string(table: &[&[u8]]) -> Option<u32> {
        match Strings::new(table) {
            Ok(_) => None,
            Err(PbfFormatError::InvalidUtf8(i)) => Some(i),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn every_string_is_checked_as_utf8() {
        let strings = Strings::new(&[b"", b"name", "Одеса".as_bytes()]).unwrap();
        assert_eq!((strings.get(1), strings.get(2)), ("name", "Одеса"));
        assert_eq!(bad_string(&[b"", b"ok", b"\xFF"]), Some(2));
        // `é` split between two strings: the joined bytes are valid UTF-8, but neither string is.
        assert_eq!(bad_string(&[b"", b"a\xC3", b"\xA9b"]), Some(1));
        assert_eq!(bad_string(&[b"", b"\xC3", b""]), Some(1));
    }

    #[cfg(feature = "nodes")]
    #[test]
    fn node_locations_match_the_nodes() {
        use std::path::Path;

        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/generated");
        // a granularity and offsets other than the default, negative IDs, and plain nodes beside dense ones
        for name in ["granularity", "negative_ids", "mixed_groups", "packed"] {
            let reader = crate::PbfReader::open(fixtures.join(format!("{name}.osm.pbf"))).unwrap();
            let mut nodes = 0;
            for block in reader.blocks(Select::node_locations()).unwrap() {
                let block = block.unwrap();
                let mut got = Vec::new();
                block.push_node_locations(&mut got);
                let want: Vec<_> = block
                    .nodes()
                    .filter_map(|n| {
                        let id = u64::try_from(n.id()).ok()?;
                        Some((
                            id,
                            crate::Location::new(n.decimicro_lat(), n.decimicro_lon()),
                        ))
                    })
                    .collect();
                assert_eq!(got, want, "{name}");
                nodes += block.nodes().len();
            }
            assert!(nodes > 0, "{name} has nodes");
        }
    }
}
