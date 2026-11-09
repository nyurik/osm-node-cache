//! Times osmflow reading a whole PBF file: on one thread, and in parallel in file order and unordered.

#![allow(
    clippy::multiple_crate_versions,
    reason = "a benchmark, never published: criterion's duplicate dependencies cost only build time"
)]

use std::path::Path;
use std::sync::Mutex;

use osm_bench::{Reader, Summary};

fn osmflow_block(block: &osmflow::Block) -> Summary {
    let mut s = Summary::default();
    for n in block.nodes() {
        s.node(n.id(), n.decimicro_lat(), n.decimicro_lon());
        s.tags(n.tags());
    }
    for w in block.ways() {
        s.way(w.id(), w.refs());
        s.tags(w.tags());
    }
    for r in block.relations() {
        s.relation(r.id());
        s.tags(r.tags());
        r.members().for_each(|m| s.member(m.role));
    }
    s
}

fn osmflow_seq(path: &Path) -> Summary {
    let file = osmflow::PbfReader::open(path).unwrap();
    let blocks = file.blocks(osmflow::Select::all()).unwrap();
    blocks.fold(Summary::default(), |s, b| {
        s.merge(osmflow_block(&b.unwrap()))
    })
}

fn osmflow_ordered(path: &Path) -> Summary {
    let file = osmflow::PbfReader::open(path).unwrap();
    let blocks = file.par_ordered(osmflow::Select::all()).unwrap();
    blocks.fold(Summary::default(), |s, b| {
        s.merge(osmflow_block(&b.unwrap()))
    })
}

fn osmflow_unordered(path: &Path) -> Summary {
    let file = osmflow::PbfReader::open(path).unwrap();
    let total = Mutex::new(Summary::default());
    file.par_unordered(osmflow::Select::all(), |block| {
        let s = osmflow_block(&block);
        let mut total = total.lock().unwrap();
        *total = total.merge(s);
        Ok(())
    })
    .unwrap();
    total.into_inner().unwrap()
}

/// The readers, by the names the benchmark prints and `report.py` reads.
const READERS: [(&str, Reader); 3] = [
    ("osmflow/one thread", osmflow_seq),
    ("osmflow/parallel ordered", osmflow_ordered),
    ("osmflow/parallel unordered", osmflow_unordered),
];

fn main() {
    osm_bench::run(&READERS);
}
