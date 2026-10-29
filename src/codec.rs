//! The encoding of node locations shared by all stores.
//!
//! Locations are 1e-7 degree integers, as in OSM PBF files, so they come back exactly. Node IDs are
//! grouped into blocks of 64 consecutive IDs. A block stores which of its IDs are present, its
//! first location as the step from its page's base location, and each further location as the step
//! from the previous one: bit-packed, as varints, or as plain integers, whichever is smallest.
//!
//! A page is a run of blocks in ID order, starting with a table of its blocks, so a lookup goes
//! straight to its block.

pub(crate) const BLOCK_SHIFT: u32 = 6;
pub(crate) const BLOCK_LEN: usize = 1 << BLOCK_SHIFT;
/// Blocks a reader keeps decoded
const READER_SLOTS: usize = 4;

/// first block (u64), bytes used (u16), block count (u16), base location (2 × i32)
pub(crate) const PAGE_HEADER: usize = 20;
/// per block: its number above the page's first block (u16), and where it ends (u16)
pub(crate) const ENTRY: usize = 4;

pub(crate) const MASK_FULL: u8 = 0;
pub(crate) const MASK_RUN: u8 = 1;
pub(crate) const MASK_BITS: u8 = 2;
pub(crate) const CODEC_RAW: u8 = 0;
pub(crate) const CODEC_VARINT: u8 = 1;
pub(crate) const CODEC_PACKED: u8 = 2;

/// A node location in 1e-7 degrees, as stored in OSM PBF files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Location {
    pub lat: i32,
    pub lon: i32,
}

impl From<(i32, i32)> for Location {
    fn from((lat, lon): (i32, i32)) -> Self {
        Self { lat, lon }
    }
}

impl Location {
    #[must_use]
    pub fn new(lat: i32, lon: i32) -> Self {
        Self { lat, lon }
    }

    /// Rounds degrees to the nearest 1e-7 degree.
    #[must_use]
    #[expect(clippy::cast_possible_truncation)]
    pub fn from_degrees(lat: f64, lon: f64) -> Self {
        Self::new((lat * 1e7).round() as i32, (lon * 1e7).round() as i32)
    }

    #[must_use]
    pub fn lat_degrees(self) -> f64 {
        f64::from(self.lat) / 1e7
    }

    #[must_use]
    pub fn lon_degrees(self) -> f64 {
        f64::from(self.lon) / 1e7
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Block {
    pub(crate) num: u64,
    pub(crate) mask: u64,
    pub(crate) locs: Vec<Location>,
}

impl Block {
    pub(crate) fn single(num: u64, bit: u64, loc: Location) -> Self {
        Self {
            num,
            mask: bit,
            locs: vec![loc],
        }
    }

    /// The block's nodes and their locations, in ID order
    pub(crate) fn nodes(&self) -> impl Iterator<Item = (u64, Location)> + '_ {
        let mut mask = self.mask;
        self.locs.iter().map(move |&loc| {
            let id = (self.num << BLOCK_SHIFT) | u64::from(mask.trailing_zeros());
            mask &= mask - 1;
            (id, loc)
        })
    }
}

/// The block number of node `id`, and the node's bit in the block's mask
#[inline]
pub(crate) fn block_of(id: u64) -> (u64, u64) {
    (id >> BLOCK_SHIFT, 1 << (id & (BLOCK_LEN as u64 - 1)))
}

/// The index of the node at `bit` among the block's present nodes
#[inline]
pub(crate) fn rank(mask: u64, bit: u64) -> usize {
    (mask & (bit - 1)).count_ones() as usize
}

pub(crate) struct PageHeader {
    pub(crate) first_block: u64,
    pub(crate) used: usize,
    pub(crate) n_blocks: usize,
    pub(crate) base: Location,
}

#[inline]
pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

#[inline]
pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[inline]
pub(crate) fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

#[inline]
pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[inline]
pub(crate) fn page_header(page: &[u8]) -> PageHeader {
    PageHeader {
        first_block: u64_at(page, 0),
        used: usize::from(u16_at(page, 8)),
        n_blocks: usize::from(u16_at(page, 10)),
        base: Location::new(i32_at(page, 12), i32_at(page, 16)),
    }
}

pub(crate) fn set_page_header(page: &mut [u8], first_block: u64, n_blocks: usize, base: Location) {
    let used = u16::try_from(page.len()).expect("page too large");
    let n_blocks = u16::try_from(n_blocks).expect("too many blocks");
    page[0..8].copy_from_slice(&first_block.to_le_bytes());
    page[8..10].copy_from_slice(&used.to_le_bytes());
    page[10..12].copy_from_slice(&n_blocks.to_le_bytes());
    page[12..16].copy_from_slice(&base.lat.to_le_bytes());
    page[16..20].copy_from_slice(&base.lon.to_le_bytes());
}

/// Block `i`'s number above its page's first block, and where it ends
#[inline]
pub(crate) fn entry(page: &[u8], i: usize) -> (u16, usize) {
    let at = PAGE_HEADER + ENTRY * i;
    (u16_at(page, at), usize::from(u16_at(page, at + 2)))
}

pub(crate) fn set_entry(page: &mut [u8], i: usize, rel: u16, end: usize) {
    let at = PAGE_HEADER + ENTRY * i;
    let end = u16::try_from(end).expect("page too large");
    page[at..at + 2].copy_from_slice(&rel.to_le_bytes());
    page[at + 2..at + 4].copy_from_slice(&end.to_le_bytes());
}

pub(crate) fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)).cast_unsigned()
}

#[inline]
pub(crate) fn unzigzag(v: u64) -> i64 {
    (v >> 1).cast_signed() ^ -((v & 1).cast_signed())
}

pub(crate) fn bit_width(v: u64) -> u32 {
    64 - v.leading_zeros()
}

pub(crate) fn varint_len(v: u64) -> usize {
    bit_width(v).max(1).div_ceil(7) as usize
}

#[expect(clippy::cast_possible_truncation)]
pub(crate) fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline]
pub(crate) fn read_varint(b: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0_u64;
    let mut shift = 0;
    loop {
        let byte = b[*pos];
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return v;
        }
        shift += 7;
    }
}

/// Reads `width` bits (at most 57) starting at bit `bit` of `b`.
#[inline]
pub(crate) fn read_bits(b: &[u8], bit: usize, width: usize) -> u64 {
    let start = bit / 8;
    let word = if let Some(w) = b.get(start..start + 8) {
        u64::from_le_bytes(w.try_into().unwrap())
    } else {
        let mut word = [0_u8; 8];
        let tail = b.get(start..).unwrap_or_default();
        word[..tail.len()].copy_from_slice(tail);
        u64::from_le_bytes(word)
    };
    (word >> (bit % 8)) & ((1 << width) - 1)
}

pub(crate) struct BitWriter<'a> {
    pub(crate) out: &'a mut Vec<u8>,
    pub(crate) acc: u64,
    pub(crate) bits: u32,
}

#[expect(clippy::cast_possible_truncation)]
impl BitWriter<'_> {
    pub(crate) fn push(&mut self, v: u64, width: u32) {
        self.acc |= v << self.bits;
        self.bits += width;
        while self.bits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }

    pub(crate) fn finish(self) {
        if self.bits > 0 {
            self.out.push(self.acc as u8);
        }
    }
}

pub(crate) fn step(from: Location, to: Location) -> (u64, u64) {
    (
        zigzag(i64::from(to.lat) - i64::from(from.lat)),
        zigzag(i64::from(to.lon) - i64::from(from.lon)),
    )
}

#[expect(clippy::cast_possible_truncation)]
#[inline]
pub(crate) fn apply(from: Location, (dlat, dlon): (u64, u64)) -> Location {
    Location::new(
        (i64::from(from.lat) + unzigzag(dlat)) as i32,
        (i64::from(from.lon) + unzigzag(dlon)) as i32,
    )
}

/// The start and length of `mask`'s bits, if they are one run shorter than a block
#[expect(clippy::cast_possible_truncation)]
pub(crate) fn as_run(mask: u64) -> Option<(u8, u8)> {
    let start = mask.trailing_zeros();
    let count = (mask >> start).trailing_ones();
    (mask >> start >> count == 0 && count < 64).then_some((start as u8, count as u8))
}

#[expect(clippy::cast_possible_truncation)]
pub(crate) fn encode_block(out: &mut Vec<u8>, b: &Block, base: Location) {
    let mut buf = [(0, 0); BLOCK_LEN - 1];
    let steps = &mut buf[..b.locs.len() - 1];
    for (s, w) in steps.iter_mut().zip(b.locs.windows(2)) {
        *s = step(w[0], w[1]);
    }
    let n = steps.len();
    let (wlat, wlon) = steps.iter().fold((0, 0), |(wlat, wlon), &(dlat, dlon)| {
        (wlat.max(bit_width(dlat)), wlon.max(bit_width(dlon)))
    });
    let varint: usize = steps
        .iter()
        .map(|&(dlat, dlon)| varint_len(dlat) + varint_len(dlon))
        .sum();
    let packed = 2 + ((wlat + wlon) as usize * n).div_ceil(8);
    let codec = if 8 * n <= varint.min(packed) {
        CODEC_RAW
    } else if varint <= packed {
        CODEC_VARINT
    } else {
        CODEC_PACKED
    };

    let kind = codec << 2;
    if b.mask == u64::MAX {
        out.push(MASK_FULL | kind);
    } else if let Some((start, count)) = as_run(b.mask) {
        out.extend_from_slice(&[MASK_RUN | kind, start, count]);
    } else {
        out.push(MASK_BITS | kind);
        out.extend_from_slice(&b.mask.to_le_bytes());
    }
    let (dlat, dlon) = step(base, b.locs[0]);
    write_varint(out, dlat);
    write_varint(out, dlon);
    match codec {
        CODEC_RAW => {
            for l in &b.locs[1..] {
                out.extend_from_slice(&l.lat.to_le_bytes());
                out.extend_from_slice(&l.lon.to_le_bytes());
            }
        }
        CODEC_VARINT => {
            for &(dlat, dlon) in &*steps {
                write_varint(out, dlat);
                write_varint(out, dlon);
            }
        }
        _ => {
            out.extend_from_slice(&[wlat as u8, wlon as u8]);
            let mut w = BitWriter {
                out,
                acc: 0,
                bits: 0,
            };
            for &(dlat, _) in &*steps {
                w.push(dlat, wlat);
            }
            for &(_, dlon) in &*steps {
                w.push(dlon, wlon);
            }
            w.finish();
        }
    }
}

/// A block's header, read from its page.
pub(crate) struct Head {
    pub(crate) num: u64,
    pub(crate) mask: u64,
    pub(crate) mask_kind: u8,
    pub(crate) codec: u8,
    pub(crate) first: Location,
    /// where the block's steps start in the page
    pub(crate) steps: usize,
}

/// The header of block `num`, if `page` has it
#[inline]
pub(crate) fn find_in_page(page: &[u8], num: u64) -> Option<Head> {
    let h = page_header(page);
    let rel = u16::try_from(num.checked_sub(h.first_block)?).ok()?;
    let table = page[PAGE_HEADER..PAGE_HEADER + ENTRY * h.n_blocks]
        .as_chunks::<ENTRY>()
        .0;
    let i = table
        .binary_search_by_key(&rel, |e| u16::from_le_bytes([e[0], e[1]]))
        .ok()?;
    Some(read_block(page, &h, i))
}

/// The header of block `i` of a page
#[inline]
pub(crate) fn read_block(page: &[u8], h: &PageHeader, i: usize) -> Head {
    let pos = if i == 0 {
        PAGE_HEADER + ENTRY * h.n_blocks
    } else {
        entry(page, i - 1).1
    };
    let kind = page[pos];
    let (mask, mask_len) = match kind & 3 {
        MASK_FULL => (u64::MAX, 0),
        MASK_RUN => (((1_u64 << page[pos + 2]) - 1) << page[pos + 1], 2),
        _ => (u64_at(page, pos + 1), 8),
    };
    let mut pos = pos + 1 + mask_len;
    let dlat = read_varint(page, &mut pos);
    let dlon = read_varint(page, &mut pos);
    Head {
        num: h.first_block + u64::from(entry(page, i).0),
        mask,
        mask_kind: kind & 3,
        codec: kind >> 2,
        first: apply(h.base, (dlat, dlon)),
        steps: pos,
    }
}

impl Head {
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.mask.count_ones() as usize
    }

    /// The location of the block's `rank`-th present node.
    #[inline]
    pub(crate) fn location(&self, page: &[u8], rank: usize) -> Location {
        if self.codec == CODEC_RAW && rank > 0 {
            return raw_location(&page[self.steps..], rank);
        }
        let mut out = self.first;
        self.decode(page, rank + 1, |_, loc| out = loc);
        out
    }

    /// Decodes the locations of the block's present nodes into the start of `locs`.
    #[inline]
    pub(crate) fn decode_into(&self, page: &[u8], locs: &mut [Location; BLOCK_LEN]) {
        locs[0] = self.first;
        self.decode(page, self.len(), |r, loc| locs[r] = loc);
    }

    /// Calls `f` with the rank and location of each present node from rank 1 to below `end`.
    #[inline]
    pub(crate) fn decode(&self, page: &[u8], end: usize, mut f: impl FnMut(usize, Location)) {
        let p = &page[self.steps..];
        let mut loc = self.first;
        match self.codec {
            CODEC_RAW => (1..end).for_each(|r| f(r, raw_location(p, r))),
            CODEC_VARINT => {
                let mut pos = 0;
                for r in 1..end {
                    let dlat = read_varint(p, &mut pos);
                    loc = apply(loc, (dlat, read_varint(p, &mut pos)));
                    f(r, loc);
                }
            }
            _ => {
                let (wlat, wlon) = (usize::from(p[0]), usize::from(p[1]));
                // the bits run on to the end of the page, so most reads need no bounds care
                let (bits, lon_start) = (&p[2..], (self.len() - 1) * wlat);
                for r in 1..end {
                    let i = r - 1;
                    let dlat = read_bits(bits, i * wlat, wlat);
                    loc = apply(loc, (dlat, read_bits(bits, lon_start + i * wlon, wlon)));
                    f(r, loc);
                }
            }
        }
    }
}

/// The plain location of rank `rank`, from a block's steps
#[inline]
pub(crate) fn raw_location(steps: &[u8], rank: usize) -> Location {
    Location::new(i32_at(steps, 8 * rank - 8), i32_at(steps, 8 * rank - 4))
}

pub(crate) fn decode_page(page: &[u8]) -> (u64, Vec<Block>) {
    let h = page_header(page);
    let blocks = (0..h.n_blocks)
        .map(|i| {
            let head = read_block(page, &h, i);
            let mut locs = [Location::default(); BLOCK_LEN];
            head.decode_into(page, &mut locs);
            Block {
                num: head.num,
                mask: head.mask,
                locs: locs[..head.len()].to_vec(),
            }
        })
        .collect();
    (h.first_block, blocks)
}

/// Encodes a page; its blocks must be within `u16::MAX` of `first_block`.
pub(crate) fn encode_page(first_block: u64, blocks: &[Block], out: &mut Vec<u8>) {
    out.clear();
    out.resize(PAGE_HEADER + ENTRY * blocks.len(), 0);
    let base = blocks.first().map_or_else(Location::default, |b| b.locs[0]);
    for (i, b) in blocks.iter().enumerate() {
        encode_block(out, b, base);
        let rel = u16::try_from(b.num - first_block).expect("block too far above its page's first");
        let end = out.len();
        set_entry(out, i, rel, end);
    }
    set_page_header(out, first_block, blocks.len(), base);
}

/// Where to split blocks that overflow a page into two halves of about equal size
pub(crate) fn split_point(blocks: &[Block]) -> usize {
    let mut tmp = Vec::new();
    let sizes: Vec<usize> = blocks
        .iter()
        .map(|b| {
            tmp.clear();
            encode_block(&mut tmp, b, blocks[0].locs[0]);
            tmp.len() + ENTRY
        })
        .collect();
    let half = sizes.iter().sum::<usize>() / 2;
    let mut sum = 0;
    let k = sizes.iter().position(|&s| {
        sum += s;
        sum >= half
    });
    k.unwrap_or(0).clamp(1, blocks.len() - 1)
}

/// Reads locations for one thread, keeping the last few blocks it read decoded, as a way's nodes
/// are mostly in the same few blocks.
pub(crate) struct Slots {
    nums: [u64; READER_SLOTS],
    masks: [u64; READER_SLOTS],
    locs: [[Location; BLOCK_LEN]; READER_SLOTS],
    next: usize,
}

impl Default for Slots {
    fn default() -> Self {
        Self {
            nums: [u64::MAX; READER_SLOTS],
            masks: [0; READER_SLOTS],
            locs: [[Location::default(); BLOCK_LEN]; READER_SLOTS],
            next: 0,
        }
    }
}

impl Slots {
    /// The location of node `id`. A block not kept is decoded into the oldest slot by `load`,
    /// which returns the block's mask (0 if it is not stored).
    #[inline]
    pub(crate) fn get(
        &mut self,
        id: u64,
        load: impl FnOnce(u64, &mut [Location; BLOCK_LEN]) -> u64,
    ) -> Option<Location> {
        let (num, bit) = block_of(id);
        let slot = self.nums.iter().position(|&n| n == num).unwrap_or_else(|| {
            let slot = self.next;
            self.next = (slot + 1) % READER_SLOTS;
            self.nums[slot] = num;
            self.masks[slot] = load(num, &mut self.locs[slot]);
            slot
        });
        let mask = self.masks[slot];
        (mask & bit != 0).then(|| self.locs[slot][rank(mask, bit)])
    }
}
