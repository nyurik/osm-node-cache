//! Builds the files in `fixtures/generated/`: cases no published file has. They are written byte by byte with a
//! minimal protobuf encoder, in uncompressed blobs, so they never change unless this code does.
//!
//! `generated_fixtures_are_current` fails if a checked-in file differs from what this code builds; run it with
//! `OSMFLOW_REGENERATE=1` to rewrite them. `tests/fixtures.rs` then reads them like every other fixture.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    reason = "a protobuf encoder reinterprets integers on purpose"
)]

use std::path::Path;

/// A protobuf message being written.
#[derive(Default)]
struct Pb(Vec<u8>);

impl Pb {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn uint(&mut self, field: u32, v: u64) -> &mut Self {
        self.varint(u64::from(field << 3));
        self.varint(v);
        self
    }

    fn int(&mut self, field: u32, v: i64) -> &mut Self {
        self.uint(field, v as u64)
    }

    fn sint(&mut self, field: u32, v: i64) -> &mut Self {
        self.uint(field, zigzag(v))
    }

    fn bytes(&mut self, field: u32, b: &[u8]) -> &mut Self {
        self.varint(u64::from(field << 3 | 2));
        self.varint(b.len() as u64);
        self.0.extend_from_slice(b);
        self
    }

    fn msg(&mut self, field: u32, m: &Pb) -> &mut Self {
        self.bytes(field, &m.0)
    }

    /// A packed array of varints.
    fn packed(&mut self, field: u32, values: &[u64]) -> &mut Self {
        let mut p = Pb::default();
        for &v in values {
            p.varint(v);
        }
        self.bytes(field, &p.0)
    }

    /// The same values, one varint field each: the unpacked form, which the format allows for a packed field.
    fn unpacked(&mut self, field: u32, values: &[u64]) -> &mut Self {
        for &v in values {
            self.uint(field, v);
        }
        self
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Delta-coded, zigzag-encoded values.
fn deltas(values: &[i64]) -> Vec<u64> {
    let mut prev = 0i64;
    values
        .iter()
        .map(|&v| {
            let d = v.wrapping_sub(prev);
            prev = v;
            zigzag(d)
        })
        .collect()
}

fn ints(values: &[i64]) -> Vec<u64> {
    values.iter().map(|&v| v as u64).collect()
}

/// A raw (uncompressed) `Blob`.
fn raw_blob(data: &[u8]) -> Pb {
    let mut b = Pb::default();
    b.bytes(1, data).uint(2, data.len() as u64);
    b
}

/// A file: each blob framed with its 4-byte length and `BlobHeader`.
fn file(blobs: &[(&str, Pb)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, blob) in blobs {
        let mut header = Pb::default();
        header
            .bytes(1, kind.as_bytes())
            .uint(3, blob.0.len() as u64);
        out.extend_from_slice(&u32::try_from(header.0.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&header.0);
        out.extend_from_slice(&blob.0);
    }
    out
}

fn header(required: &[&str]) -> (&'static str, Pb) {
    let mut h = Pb::default();
    for f in required {
        h.bytes(4, f.as_bytes());
    }
    h.bytes(16, b"osmflow tests/generated.rs");
    ("OSMHeader", raw_blob(&h.0))
}

const STRINGS: [&str; 7] = [
    "",
    "highway",
    "residential",
    "name",
    "Main St",
    "outer",
    "user_a",
];

fn strings() -> Pb {
    let mut s = Pb::default();
    for v in STRINGS {
        s.bytes(1, v.as_bytes());
    }
    s
}

/// A block: the string table, then the groups.
fn block(groups: &[Pb], extra: impl FnOnce(&mut Pb)) -> (&'static str, Pb) {
    let mut b = Pb::default();
    b.msg(1, &strings());
    for g in groups {
        b.msg(2, g);
    }
    extra(&mut b);
    ("OSMData", raw_blob(&b.0))
}

/// Writes a packed field's values: packed, unpacked or split.
type Pack = fn(&mut Pb, u32, &[u64]);

/// Dense metadata for `n` nodes: versions 1.., timestamps 1000 s apart, changeset 7, user 6 ("`user_a`").
fn dense_info(n: usize, pack: Pack) -> Pb {
    let versions: Vec<i64> = (1..=n as i64).collect();
    let times: Vec<i64> = (0..n as i64).map(|i| 1_600_000_000 + i * 1000).collect();
    let mut i = Pb::default();
    pack(&mut i, 1, &ints(&versions));
    pack(&mut i, 2, &deltas(&times));
    pack(&mut i, 3, &deltas(&vec![7; n]));
    pack(&mut i, 4, &deltas(&vec![42; n]));
    pack(&mut i, 5, &deltas(&vec![6; n]));
    i
}

fn info() -> Pb {
    let mut i = Pb::default();
    i.uint(1, 3)
        .uint(2, 1_600_000_000)
        .uint(3, 7)
        .uint(4, 42)
        .uint(5, 6);
    i
}

fn way(id: i64, refs: &[i64], pack: Pack) -> Pb {
    let mut w = Pb::default();
    w.int(1, id);
    pack(&mut w, 2, &[1]);
    pack(&mut w, 3, &[2]);
    w.msg(4, &info());
    pack(&mut w, 8, &deltas(refs));
    w
}

fn relation(id: i64, way: i64, pack: Pack) -> Pb {
    let mut r = Pb::default();
    r.int(1, id);
    pack(&mut r, 2, &[3]);
    pack(&mut r, 3, &[4]);
    r.msg(4, &info());
    pack(&mut r, 8, &[5]);
    pack(&mut r, 9, &deltas(&[way]));
    pack(&mut r, 10, &[1]);
    r
}

const IDS: [i64; 3] = [1, 2, 3];
const LATS: [i64; 3] = [400_000_000, 400_000_100, 399_999_900];
const LONS: [i64; 3] = [-740_000_000, -740_000_200, -739_999_800];

fn dense(pack: Pack) -> Pb {
    let mut d = Pb::default();
    pack(&mut d, 1, &deltas(&IDS));
    d.msg(5, &dense_info(3, pack));
    pack(&mut d, 8, &deltas(&LATS));
    pack(&mut d, 9, &deltas(&LONS));
    pack(&mut d, 10, &[1, 2, 0, 0, 3, 4, 0]);
    d
}

fn group(field: u32, items: &[Pb]) -> Pb {
    let mut g = Pb::default();
    for item in items {
        g.msg(field, item);
    }
    g
}

fn packed(p: &mut Pb, field: u32, values: &[u64]) {
    p.packed(field, values);
}

fn unpacked(p: &mut Pb, field: u32, values: &[u64]) {
    p.unpacked(field, values);
}

/// Each packed field in two parts: the first value, then the rest.
fn split(p: &mut Pb, field: u32, values: &[u64]) {
    let (a, b) = values.split_at(values.len().min(1));
    p.packed(field, a);
    if !b.is_empty() {
        p.packed(field, b);
    }
}

fn standard(pack: Pack) -> Vec<u8> {
    file(&[
        header(&["OsmSchema-V0.6", "DenseNodes"]),
        block(&[group(2, &[dense(pack)])], |_| {}),
        block(&[group(3, &[way(10, &IDS, pack)])], |_| {}),
        block(&[group(4, &[relation(20, 10, pack)])], |_| {}),
    ])
}

#[allow(clippy::too_many_lines, reason = "one list of fixtures")]
fn build_all() -> Vec<(&'static str, Vec<u8>)> {
    let mut all = vec![
        ("packed.osm.pbf", standard(packed)),
        ("unpacked.osm.pbf", standard(unpacked)),
        ("split_packed.osm.pbf", standard(split)),
    ];

    // One block with every kind of group, plain nodes included.
    let mut plain = Pb::default();
    plain
        .sint(1, 5)
        .packed(2, &[3])
        .packed(3, &[4])
        .msg(4, &info())
        .sint(8, 410_000_000)
        .sint(9, -730_000_000);
    all.push((
        "mixed_groups.osm.pbf",
        file(&[
            header(&["OsmSchema-V0.6", "DenseNodes"]),
            block(
                &[
                    group(1, &[plain]),
                    group(2, &[dense(packed)]),
                    group(3, &[way(10, &IDS, packed)]),
                    group(4, &[relation(20, 10, packed)]),
                ],
                |_| {},
            ),
        ]),
    ));

    // Non-default granularity (1000 nanodegrees, so the stored values are a tenth of the usual), offsets and date
    // granularity.
    let mut coarse = Pb::default();
    let tenth = |v: &[i64]| v.iter().map(|x| x / 10).collect::<Vec<_>>();
    coarse
        .packed(1, &deltas(&IDS))
        .msg(5, &dense_info(3, packed))
        .packed(8, &deltas(&tenth(&LATS)))
        .packed(9, &deltas(&tenth(&LONS)))
        .packed(10, &[1, 2, 0, 0, 3, 4, 0]);
    all.push((
        "granularity.osm.pbf",
        file(&[
            header(&["OsmSchema-V0.6", "DenseNodes"]),
            block(&[group(2, &[coarse])], |b| {
                b.uint(17, 1000).uint(18, 1).int(19, 500).int(20, -700);
            }),
        ]),
    ));

    // Negative IDs, as editors write for new objects.
    let mut neg = Pb::default();
    neg.packed(1, &deltas(&[-1, -2, -3]))
        .packed(8, &deltas(&LATS))
        .packed(9, &deltas(&LONS));
    all.push((
        "negative_ids.osm.pbf",
        file(&[
            header(&["OsmSchema-V0.6", "DenseNodes"]),
            block(&[group(2, &[neg])], |_| {}),
            block(&[group(3, &[way(-5, &[-1, -2, -3], packed)])], |_| {}),
            block(&[group(4, &[relation(-7, -5, packed)])], |_| {}),
        ]),
    ));

    // Compression osmflow rejects: the data is not decompressed, so it need not be real zstd or lzma.
    for (name, field) in [("zstd.osm.pbf", 7), ("lzma.osm.pbf", 4)] {
        let mut blob = Pb::default();
        blob.bytes(field, b"not decompressed").uint(2, 100);
        all.push((
            name,
            file(&[header(&["OsmSchema-V0.6"]), ("OSMData", blob)]),
        ));
    }

    // A file cut inside its last blob.
    let mut cut = standard(packed);
    cut.truncate(cut.len() - 5);
    all.push(("truncated.osm.pbf", cut));

    // A blob header length over the 64 KiB limit.
    let mut big = file(&[header(&["OsmSchema-V0.6"])]);
    big.extend_from_slice(&(1u32 << 20).to_be_bytes());
    big.extend_from_slice(&[0; 16]);
    all.push(("header_too_large.osm.pbf", big));

    // A raw blob whose declared size is not its length.
    let mut wrong = Pb::default();
    wrong.bytes(1, b"abc").uint(2, 999);
    all.push((
        "raw_size_mismatch.osm.pbf",
        file(&[header(&["OsmSchema-V0.6"]), ("OSMData", wrong)]),
    ));

    // A tag key beyond the string table.
    let mut bad = Pb::default();
    bad.int(1, 10)
        .packed(2, &[99])
        .packed(3, &[2])
        .packed(8, &deltas(&IDS));
    all.push((
        "bad_string_index.osm.pbf",
        file(&[
            header(&["OsmSchema-V0.6"]),
            block(&[group(3, &[bad])], |_| {}),
        ]),
    ));

    // Dense tags for two nodes, where there are three.
    let mut short = Pb::default();
    short
        .packed(1, &deltas(&IDS))
        .packed(8, &deltas(&LATS))
        .packed(9, &deltas(&LONS))
        .packed(10, &[1, 2, 0, 0]);
    all.push((
        "dense_tags_mismatch.osm.pbf",
        file(&[
            header(&["OsmSchema-V0.6", "DenseNodes"]),
            block(&[group(2, &[short])], |_| {}),
        ]),
    ));

    // A required feature osmflow does not know.
    all.push((
        "unknown_feature.osm.pbf",
        file(&[header(&["OsmSchema-V0.6", "Sort.Teleport"])]),
    ));

    // Dense tags ending with a key that has no value.
    let mut lone = Pb::default();
    lone.packed(1, &deltas(&IDS[..1]))
        .packed(8, &deltas(&LATS[..1]))
        .packed(9, &deltas(&LONS[..1]))
        .packed(10, &[1]);
    all.push((
        "dense_key_without_value.osm.pbf",
        file(&[
            header(&["OsmSchema-V0.6", "DenseNodes"]),
            block(&[group(2, &[lone])], |_| {}),
        ]),
    ));

    // Relation members with a type that is not node, way or relation, and with a negative role index.
    for (name, role, kind) in [
        ("bad_member_type.osm.pbf", 5, 7),
        ("negative_role.osm.pbf", (-1i64) as u64, 1),
    ] {
        let mut r = Pb::default();
        r.int(1, 20)
            .packed(8, &[role])
            .packed(9, &deltas(&[10]))
            .packed(10, &[kind]);
        all.push((
            name,
            file(&[
                header(&["OsmSchema-V0.6"]),
                block(&[group(4, &[r])], |_| {}),
            ]),
        ));
    }

    // A way with three refs and two locations.
    let mut short_locs = Pb::default();
    short_locs
        .int(1, 10)
        .packed(8, &deltas(&IDS))
        .packed(9, &deltas(&LATS[..2]))
        .packed(10, &deltas(&LONS[..2]));
    all.push((
        "way_locations_mismatch.osm.pbf",
        file(&[
            header(&["OsmSchema-V0.6", "LocationsOnWays"]),
            block(&[group(3, &[short_locs])], |_| {}),
        ]),
    ));

    // A coordinate granularity of 0 and a negative date granularity, which no file may have.
    for (name, field, value) in [
        ("zero_granularity.osm.pbf", 17, 0),
        ("negative_date_granularity.osm.pbf", 18, -1),
    ] {
        all.push((
            name,
            file(&[
                header(&["OsmSchema-V0.6", "DenseNodes"]),
                block(&[group(2, &[dense(packed)])], |b| {
                    b.int(field, value);
                }),
            ]),
        ));
    }

    // Nodes again after a way: not sorted by type, so an unordered read has a barrier at every block.
    all.push((
        "unsorted_types.osm.pbf",
        file(&[
            header(&["OsmSchema-V0.6", "DenseNodes"]),
            block(&[group(2, &[dense(packed)])], |_| {}),
            block(&[group(3, &[way(10, &IDS, packed)])], |_| {}),
            block(&[group(2, &[dense(packed)])], |_| {}),
            block(&[group(4, &[relation(20, 10, packed)])], |_| {}),
        ]),
    ));

    // Metadata with no user, in a block whose string table is empty: the user is the empty string, not an index
    // outside the table.
    let mut anonymous = Pb::default();
    anonymous.uint(1, 2).uint(2, 1_600_000_000).uint(3, 7);
    let mut way = Pb::default();
    way.int(1, 10).msg(4, &anonymous).packed(8, &deltas(&IDS));
    let mut data = Pb::default();
    data.msg(1, &Pb::default()).msg(2, &group(3, &[way]));
    all.push((
        "info_without_user.osm.pbf",
        file(&[header(&["OsmSchema-V0.6"]), ("OSMData", raw_blob(&data.0))]),
    ));

    // A string table entry that is not UTF-8, used as a tag value.
    let mut table = Pb::default();
    table.bytes(1, b"").bytes(1, b"name").bytes(1, b"\xff");
    let mut way = Pb::default();
    way.int(1, 10)
        .packed(2, &[1])
        .packed(3, &[2])
        .packed(8, &deltas(&IDS));
    let mut data = Pb::default();
    data.msg(1, &table).msg(2, &group(3, &[way]));
    all.push((
        "invalid_utf8.osm.pbf",
        file(&[header(&["OsmSchema-V0.6"]), ("OSMData", raw_blob(&data.0))]),
    ));

    all
}

#[test]
fn generated_fixtures_are_current() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/generated");
    let regenerate = std::env::var_os("OSMFLOW_REGENERATE").is_some();
    if regenerate {
        std::fs::create_dir_all(&dir).unwrap();
    }
    for (name, bytes) in build_all() {
        let path = dir.join(name);
        if regenerate {
            std::fs::write(&path, &bytes).unwrap();
        } else {
            let current = std::fs::read(&path).unwrap_or_default();
            assert!(
                current == bytes,
                "{name} is out of date: run with OSMFLOW_REGENERATE=1"
            );
        }
    }
}
