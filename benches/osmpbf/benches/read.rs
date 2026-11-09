//! Times osmpbf reading a whole PBF file: on one thread, and with `par_map_reduce`.

#![allow(
    clippy::multiple_crate_versions,
    reason = "a benchmark, never published: criterion's duplicate dependencies cost only build time"
)]

use std::path::Path;

use osm_bench::{Reader, Summary};

/// Adds `e` to `s` in place, as the osmflow reader does, so the single-thread reader pays for no summary per element.
fn osmpbf_element(s: &mut Summary, e: &osmpbf::Element<'_>) {
    use osmpbf::Element;
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
}

fn osmpbf_seq(path: &Path) -> Summary {
    let mut total = Summary::default();
    osmpbf::ElementReader::from_path(path)
        .unwrap()
        .for_each(|e| osmpbf_element(&mut total, &e))
        .unwrap();
    total
}

fn osmpbf_par(path: &Path) -> Summary {
    osmpbf::ElementReader::from_path(path)
        .unwrap()
        // The API maps each element on its own, so a summary per element is unavoidable here.
        .par_map_reduce(
            |e| {
                let mut s = Summary::default();
                osmpbf_element(&mut s, &e);
                s
            },
            Summary::default,
            Summary::merge,
        )
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
