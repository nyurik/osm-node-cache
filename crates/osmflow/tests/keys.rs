//! Looking up tags by handles of their keys in a block's string table.

use std::path::Path;

use osmflow::{Block, PbfError, PbfReader, Select};

fn blocks(name: &str) -> Vec<Block> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/generated")
        .join(name);
    let file = PbfReader::open(path).unwrap();
    file.blocks(Select::all())
        .unwrap()
        .collect::<Result<_, PbfError>>()
        .unwrap()
}

#[test]
#[should_panic(expected = "another block")]
fn a_key_of_another_block_panics() {
    let blocks = blocks("packed.osm.pbf");
    let highway = blocks[0].tag_key("highway").unwrap();
    let way = blocks[1].ways().next().unwrap();
    let _ = way.tag(highway);
}
