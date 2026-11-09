//! Times osmpbfreader reading a whole PBF file: on one thread, and with `par_iter`.

#![allow(
    clippy::multiple_crate_versions,
    reason = "a benchmark, never published: criterion's duplicate dependencies cost only build time"
)]

use std::fs::File;
use std::path::Path;

use osm_bench::{Reader, Summary};

fn osmpbfreader_obj(obj: &osmpbfreader::OsmObj) -> Summary {
    use osmpbfreader::OsmObj;
    let mut s = Summary::default();
    match obj {
        OsmObj::Node(n) => {
            s.node(n.id.0, n.decimicro_lat, n.decimicro_lon);
            s.tags(n.tags.iter());
        }
        OsmObj::Way(w) => {
            s.way(w.id.0, w.nodes.iter().map(|n| n.0));
            s.tags(w.tags.iter());
        }
        OsmObj::Relation(r) => {
            s.relation(r.id.0);
            s.tags(r.tags.iter());
            r.refs.iter().for_each(|m| s.member(&m.role));
        }
    }
    s
}

fn osmpbfreader_seq(path: &Path) -> Summary {
    let mut reader = osmpbfreader::OsmPbfReader::new(File::open(path).unwrap());
    reader.iter().fold(Summary::default(), |s, o| {
        s.merge(osmpbfreader_obj(&o.unwrap()))
    })
}

fn osmpbfreader_par(path: &Path) -> Summary {
    let mut reader = osmpbfreader::OsmPbfReader::new(File::open(path).unwrap());
    reader.par_iter().fold(Summary::default(), |s, o| {
        s.merge(osmpbfreader_obj(&o.unwrap()))
    })
}

/// The readers, by the names the benchmark prints and `report.py` reads.
const READERS: [(&str, Reader); 2] = [
    ("osmpbfreader/one thread", osmpbfreader_seq),
    ("osmpbfreader/par_iter", osmpbfreader_par),
];

fn main() {
    osm_bench::run(&READERS);
}
