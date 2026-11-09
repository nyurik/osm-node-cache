//! How blocks are stored: their blobs' sizes and compression, string tables, granularity and groups.

use std::path::Path;

use osmflow::{Block, BlockLayout, Group, GroupKind, PbfError, PbfReader, Select, TagMode};

fn blocks<T: TagMode>(name: &str, select: Select<T>) -> Vec<Block<T>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/generated")
        .join(name);
    PbfReader::open(path)
        .unwrap()
        .blocks(select)
        .unwrap()
        .collect::<Result<_, PbfError>>()
        .unwrap()
}

fn group(kind: GroupKind, objects: usize) -> Group {
    Group {
        kind,
        objects: Some(objects),
    }
}

#[test]
fn a_block_lists_its_groups() {
    let [plain] = <[_; 1]>::try_from(blocks("mixed_groups.osm.pbf", Select::all())).unwrap();
    let [zlib] = <[_; 1]>::try_from(blocks("mixed_groups_zlib.osm.pbf", Select::all())).unwrap();
    let groups = vec![
        group(GroupKind::Nodes, 1),
        group(GroupKind::DenseNodes, 3),
        group(GroupKind::Ways, 1),
        group(GroupKind::Relations, 1),
    ];
    let l: &BlockLayout = plain.layout();
    assert_eq!(l.groups, groups);
    assert!(!l.zlib && l.raw_bytes > 0 && l.blob_bytes > l.raw_bytes);
    assert_eq!((l.strings, l.string_bytes), (7, 40));
    assert_eq!(
        (
            l.granularity,
            l.lat_offset,
            l.lon_offset,
            l.date_granularity
        ),
        (100, 0, 0, 1000)
    );

    let z = zlib.layout();
    assert!(z.zlib && z.blob_bytes != z.raw_bytes);
    assert_eq!((z.raw_bytes, &z.groups), (l.raw_bytes, &groups));
}

#[test]
fn dense_nodes_are_counted_only_when_decoded() {
    let [block] = <[_; 1]>::try_from(blocks(
        "mixed_groups.osm.pbf",
        Select::all().without_nodes(),
    ))
    .unwrap();
    let dense = block.layout().groups[1];
    assert_eq!((dense.kind, dense.objects), (GroupKind::DenseNodes, None));
    assert_eq!(block.layout().groups[0], group(GroupKind::Nodes, 1));
}

#[test]
fn granularity_and_offsets_are_the_blocks() {
    let [block] = <[_; 1]>::try_from(blocks("granularity.osm.pbf", Select::all())).unwrap();
    let l = block.layout();
    assert_eq!(
        (
            l.granularity,
            l.lat_offset,
            l.lon_offset,
            l.date_granularity
        ),
        (1000, 500, -700, 1)
    );
}
