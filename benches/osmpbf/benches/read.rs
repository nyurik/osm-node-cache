//! Times osmpbf reading a whole PBF file: on one thread, and with `par_map_reduce`.

#![allow(
    clippy::multiple_crate_versions,
    reason = "a benchmark, never published: criterion's duplicate dependencies cost only build time"
)]

use std::path::Path;

use osm_bench::{Reader, Summary};

fn osmpbf_element(e: &osmpbf::Element<'_>) -> Summary {
    use osmpbf::Element;
    let mut s = Summary::default();
    match e {
        Element::Node(n) => {
            s.node(n.id(), n.decimicro_lat(), n.decimicro_lon());
            s.tags(n.tags());
        }
        Element::DenseNode(n) => {
            s.node(n.id(), n.decimicro_lat(), n.decimicro_lon());
            s.tags(n.tags());
        }
        Element::Way(w) => {
            s.way(w.id(), w.refs());
            s.tags(w.tags());
        }
        Element::Relation(r) => {
            s.relation(r.id());
            s.tags(r.tags());
            r.members().for_each(|m| s.member(m.role().unwrap()));
        }
    }
    s
}

fn osmpbf_seq(path: &Path) -> Summary {
    let mut total = Summary::default();
    osmpbf::ElementReader::from_path(path)
        .unwrap()
        .for_each(|e| total = total.merge(osmpbf_element(&e)))
        .unwrap();
    total
}

fn osmpbf_par(path: &Path) -> Summary {
    osmpbf::ElementReader::from_path(path)
        .unwrap()
        .par_map_reduce(|e| osmpbf_element(&e), Summary::default, Summary::merge)
        .unwrap()
}

/// The readers, by the names the benchmark prints and `report.py` reads.
const READERS: [(&str, Reader); 2] = [
    ("osmpbf/one thread", osmpbf_seq),
    ("osmpbf/par_map_reduce", osmpbf_par),
];

fn main() {
    osm_bench::run(&READERS);
}
