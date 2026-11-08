//! A decoded block of objects, held as columns, and the views of its nodes, ways and relations.

use std::borrow::Cow;
use std::ops::Range;

use buffa::MessageView as _;

use crate::error::{Error, FormatError, Position, Result};
use crate::generated::raw::osmflow_raw::{
    DenseInfoView, DenseNodeLocationsView, DenseNodesView, PrimitiveBlockView,
};
use crate::packed::{self, Value};

type FResult<T> = std::result::Result<T, FormatError>;

/// Which objects and fields to decode. Anything not selected is skipped without being decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent switches")]
pub struct Select {
    pub nodes: bool,
    pub ways: bool,
    pub relations: bool,
    /// Tags of the selected objects.
    pub tags: bool,
    /// Version, timestamp, changeset, user and visibility of the selected objects.
    pub metadata: bool,
}

impl Select {
    /// Every object and field.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            nodes: true,
            ways: true,
            relations: true,
            tags: true,
            metadata: true,
        }
    }

    /// Node IDs and locations only.
    #[must_use]
    pub const fn node_locations() -> Self {
        Self {
            nodes: true,
            ways: false,
            relations: false,
            tags: false,
            metadata: false,
        }
    }

    fn strings(self) -> bool {
        self.tags || self.metadata || self.relations
    }
}

/// The type of a relation member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemberType {
    Node,
    Way,
    Relation,
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
    fn new(table: &[&[u8]]) -> FResult<Self> {
        let mut text = String::with_capacity(table.iter().map(|s| s.len()).sum());
        let mut ends = Vec::with_capacity(table.len());
        for (i, s) in table.iter().enumerate() {
            text.push_str(std::str::from_utf8(s).map_err(|_| FormatError::InvalidUtf8(end(i)))?);
            ends.push(end(text.len()));
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
        Err(FormatError::StringIndex(index))
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
struct Tags {
    ends: Vec<u32>,
    keys: Vec<u32>,
    vals: Vec<u32>,
}

impl Tags {
    /// Ends one object's tags, once its keys and values from `first` on are appended: they must pair up, and name
    /// strings of the block's string table.
    fn end_object(&mut self, first: usize, strings: usize) -> FResult<()> {
        if self.keys.len() != self.vals.len() {
            return Err(FormatError::LengthMismatch("tag keys and values"));
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
            let v = u32::try_from(v).map_err(|_| FormatError::Packed("dense node tags"))?;
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
            return Err(FormatError::DenseTags("a key has no value"));
        }
        if self.ends.len() - first != count {
            return Err(FormatError::DenseTags(
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
    select: Select,
    /// The number of strings in the block's string table.
    strings: usize,
    date_granularity: i64,
}

impl Ctx {
    /// Decodes a plain object's `Info` message.
    fn info(&self, msg: &[u8]) -> FResult<RawInfo> {
        let bad = || FormatError::Message("info");
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
        let v = DenseInfoView::decode_view(info).map_err(FormatError::protobuf("dense info"))?;
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
        return Err(FormatError::LengthMismatch(
            "dense nodes and their metadata",
        ));
    }
    for (slot, &v) in infos.iter_mut().flatten().zip(values) {
        set(slot, v).ok_or(FormatError::Packed(what))?;
    }
    Ok(())
}

/// The columns every object type has.
#[derive(Debug, Default)]
struct Objects {
    ids: Vec<i64>,
    /// Empty unless tags are selected.
    tags: Tags,
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
            (1 | 4, _) => Err(FormatError::Message(what)),
            (field, v) => own(field, v),
        })?;
        self.ids.push(id.ok_or(FormatError::MissingId)?);
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
    out.push(u32::try_from(v).map_err(|_| FormatError::Packed(what))?);
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
                (8 | 9, _) => return Err(FormatError::Message("node")),
                _ => {}
            }
            Ok(())
        })?;
        self.lats
            .push(lat.ok_or(FormatError::NodeLocation("latitude"))?);
        self.lons
            .push(lon.ok_or(FormatError::NodeLocation("longitude"))?);
        Ok(())
    }

    /// Decodes a group's dense nodes: all their fields, or only their locations when no tags or metadata are
    /// selected, which skips the rest unread.
    ///
    /// buffa's views decode each packed array into a `Vec`, which is read again here. Borrowed packed views
    /// (`PackedView`, <https://github.com/anthropics/buffa/issues/814>) would decode the arrays as they are read here,
    /// with no `Vec` per array; osmflow can use them once a buffa release has them.
    fn push_dense(&mut self, dense: &[u8], ctx: &Ctx) -> FResult<()> {
        let bad = FormatError::protobuf("dense nodes");
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
            return Err(FormatError::LengthMismatch(
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
    ref_ends: Vec<u32>,
    refs: Vec<i64>,
    /// Locations on ways: way `i` has none if `range(loc_ends, i)` is empty, and one per ref otherwise.
    loc_ends: Vec<u32>,
    lats: Vec<i64>,
    lons: Vec<i64>,
}

impl Ways {
    fn push(&mut self, msg: &[u8], ctx: &Ctx) -> FResult<()> {
        let (first, loc) = (self.refs.len(), self.lats.len());
        let (refs, lats, lons) = (&mut self.refs, &mut self.lats, &mut self.lons);
        let (mut r, mut lat, mut lon) = (0i64, 0i64, 0i64);
        self.objects
            .push(msg, "way", false, ctx, |field, value| match field {
                8 => packed::values(value, "way refs", |v| {
                    r = r.wrapping_add(packed::zigzag(v));
                    refs.push(r);
                    Ok(())
                }),
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
        let n = self.refs.len() - first;
        let (n_lat, n_lon) = (self.lats.len() - loc, self.lons.len() - loc);
        if (n_lat, n_lon) != (0, 0) && (n_lat, n_lon) != (n, n) {
            return Err(FormatError::LengthMismatch("way refs and their locations"));
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
                        _ => return Err(FormatError::MemberType(t)),
                    });
                    Ok(())
                }),
                _ => Ok(()),
            })?;
        let len = self.member_ids.len();
        if self.member_roles.len() != len || self.member_types.len() != len {
            return Err(FormatError::LengthMismatch("member ids, roles and types"));
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
        Err(FormatError::Granularity { what, value })
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
    fn decode(data: &[u8], select: Select) -> FResult<Self> {
        let view = PrimitiveBlockView::decode_view(data).map_err(FormatError::protobuf("block"))?;
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
            if select.nodes {
                for node in &group.nodes {
                    c.nodes.push(node, &ctx)?;
                }
                if let Some(dense) = merged(group.dense.iter().copied()) {
                    c.nodes.push_dense(&dense, &ctx)?;
                }
            }
            if select.ways {
                for way in &group.ways {
                    c.ways.push(way, &ctx)?;
                }
            }
            if select.relations {
                for relation in &group.relations {
                    c.relations.push(relation, &ctx)?;
                }
            }
        }
        Ok(c)
    }
}

/// A decoded block: its objects as columns, and its string table. It borrows nothing.
#[derive(Debug)]
pub struct Block {
    position: Position,
    c: Columns,
}

impl Block {
    /// Decodes a decompressed `PrimitiveBlock`.
    pub(crate) fn decode(position: Position, data: &[u8], select: Select) -> Result<Self> {
        Ok(Self {
            position,
            c: Columns::decode(data, select).map_err(|kind| Error::format(position, kind))?,
        })
    }

    /// Where the block is in its file.
    #[must_use]
    pub fn position(&self) -> Position {
        self.position
    }

    /// The block's nodes, plain and dense, in file order.
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = Node<'_>> {
        (0..self.c.nodes.objects.ids.len()).map(move |i| Node { block: self, i })
    }

    /// The block's ways, in file order.
    pub fn ways(&self) -> impl ExactSizeIterator<Item = Way<'_>> {
        (0..self.c.ways.objects.ids.len()).map(move |i| Way { block: self, i })
    }

    /// The block's relations, in file order.
    pub fn relations(&self) -> impl ExactSizeIterator<Item = Relation<'_>> {
        (0..self.c.relations.objects.ids.len()).map(move |i| Relation { block: self, i })
    }

    fn nano(&self, raw: i64, offset: i64) -> i64 {
        offset.wrapping_add(self.c.granularity.wrapping_mul(raw))
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

    fn info(&self, objects: &Objects, i: usize) -> Option<Info<'_>> {
        let raw = (*objects.infos.get(i)?)?;
        Some(Info {
            version: raw.version,
            timestamp_ms: raw.timestamp_ms,
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
    /// When this version was made, in milliseconds since 1970.
    pub timestamp_ms: i64,
    pub changeset: i64,
    pub uid: i32,
    pub user: &'a str,
    /// False for a deleted object, in a history file.
    pub visible: bool,
}

/// A view of one object of a [`Block`], with the accessors every object type has.
macro_rules! object_view {
    ($(#[$doc:meta])* $name:ident, $column:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name<'a> {
            block: &'a Block,
            i: usize,
        }

        impl<'a> $name<'a> {
            #[must_use]
            pub fn id(&self) -> i64 {
                self.block.c.$column.objects.ids[self.i]
            }

            /// The tags, as (key, value) pairs in file order; none if tags were not selected.
            pub fn tags(&self) -> impl ExactSizeIterator<Item = (&'a str, &'a str)> + Clone + use<'a> {
                self.block.tags(&self.block.c.$column.objects, self.i)
            }

            /// The metadata, if it was selected and the file has it.
            #[must_use]
            pub fn info(&self) -> Option<Info<'a>> {
                self.block.info(&self.block.c.$column.objects, self.i)
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

impl Node<'_> {
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

impl<'a> Way<'a> {
    /// The IDs of the way's nodes.
    #[must_use]
    pub fn refs(&self) -> &'a [i64] {
        let w = &self.block.c.ways;
        &w.refs[range(&w.ref_ends, self.i)]
    }

    /// The locations of the way's nodes, in nanodegrees (latitude, longitude), when the file has them
    /// (`LocationsOnWays`).
    #[must_use]
    pub fn locations(&self) -> Option<impl ExactSizeIterator<Item = (i64, i64)> + use<'a>> {
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

/// A relation member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member<'a> {
    pub kind: MemberType,
    pub id: i64,
    pub role: &'a str,
}

impl<'a> Relation<'a> {
    /// The relation's members, in order.
    pub fn members(&self) -> impl ExactSizeIterator<Item = Member<'a>> + use<'a> {
        let block = self.block;
        let r = &block.c.relations;
        range(&r.member_ends, self.i).map(move |m| Member {
            kind: r.member_types[m],
            id: r.member_ids[m],
            role: block.c.strings.get(r.member_roles[m]),
        })
    }
}
