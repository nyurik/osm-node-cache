//! Reading a file range by range gives the same blocks, in the same order, as reading it whole.

use std::path::Path;

use osmflow::{Block, PbfReader, Select};

fn monaco() -> PbfReader {
    PbfReader::open(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/planetiler/monaco-latest.osm.pbf"),
    )
    .unwrap()
}

fn ids(block: &Block) -> Vec<i64> {
    let nodes = block.nodes().map(|n| n.id());
    let ways = block.ways().map(|w| w.id());
    nodes
        .chain(ways)
        .chain(block.relations().map(|r| r.id()))
        .collect()
}

fn whole(file: &PbfReader) -> Vec<Vec<i64>> {
    file.blocks(Select::all())
        .unwrap()
        .map(|block| ids(&block.unwrap()))
        .collect()
}

fn by_range(file: &PbfReader, bytes: u64) -> Vec<Vec<i64>> {
    file.split(bytes)
        .unwrap()
        .into_iter()
        .flat_map(|range| file.blocks_in(range, Select::all()).unwrap())
        .map(|block| ids(&block.unwrap()))
        .collect()
}

#[test]
fn ranges_cover_every_block_once_in_file_order() {
    let file = monaco();
    let expected = whole(&file);
    assert!(expected.len() > 3);
    for bytes in [0, 1, 100_000, 1 << 20, u64::MAX] {
        assert_eq!(by_range(&file, bytes), expected, "ranges of {bytes} bytes");
    }
}

#[test]
fn ranges_are_consecutive_and_hold_one_blob_or_more() {
    let file = monaco();
    let ranges = file.split(0).unwrap();
    assert_eq!(ranges.len(), whole(&file).len());
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].end, pair[1].start.offset);
        assert_eq!(pair[0].start.blob + 1, pair[1].start.blob);
    }
    let one = file.split(u64::MAX).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].start, ranges[0].start);
    assert_eq!(one[0].end, ranges.last().unwrap().end);
}

#[test]
fn filtered_ranges_skip_what_a_filtered_whole_read_skips() {
    let file = monaco()
        .nodes_where(|_| false)
        .ways_where(|s| s.has("highway"))
        .relations_where(|_| false);
    assert_eq!(by_range(&file, 0), whole(&file));
    assert_eq!(by_range(&file, 200_000), whole(&file));
}
