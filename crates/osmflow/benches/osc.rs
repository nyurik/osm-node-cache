//! Parses an osmChange file with [`OscReader`], from its XML in memory and from the gzipped file.
//!
//! `OSMFLOW_BENCH_OSC` names the file (`.osc` or `.osc.gz`); the real minute diff in `fixtures/osc` by default, which is
//! too small to say much. An hour or a day diff of the planet is a fair test:
//!
//! ```sh
//! curl -L -o hour.osc.gz https://planet.openstreetmap.org/replication/hour/000/124/081.osc.gz
//! OSMFLOW_BENCH_OSC=hour.osc.gz cargo bench -p osmflow --bench osc
//! ```
//!
//! Every change is read completely: tag keys and values and member roles go through `black_box`, and the counts are
//! printed once, with the XML's size, so the throughput can be given in changes per second as well as bytes.

use std::env;
use std::fs::File;
use std::hint::black_box;
use std::io::{BufReader, Read as _};
use std::path::PathBuf;
use std::time::Instant;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use flate2::read::MultiGzDecoder;
use osmflow::osc::{Change, Object, OscError, OscReader};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Summary {
    changes: u64,
    tags: u64,
    refs: u64,
    members: u64,
    string_bytes: u64,
}

impl Summary {
    fn add(&mut self, change: &Change) {
        self.changes += 1;
        for (k, v) in change.object.tags().iter() {
            self.tags += 1;
            self.string_bytes += (black_box(k).len() + black_box(v).len()) as u64;
        }
        match &change.object {
            Object::Node(n) => {
                black_box(n.location);
            }
            Object::Way(w) => self.refs += black_box(&w.refs).len() as u64,
            Object::Relation(r) => {
                for m in r.members() {
                    self.members += 1;
                    self.string_bytes += black_box(m.role).len() as u64;
                }
            }
        }
    }
}

fn summarize(changes: impl Iterator<Item = Result<Change, OscError>>) -> Summary {
    let mut summary = Summary::default();
    for change in changes {
        summary.add(&change.unwrap());
    }
    summary
}

fn path() -> PathBuf {
    env::var_os("OSMFLOW_BENCH_OSC").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures/osc/planet-minute-7363600.osc.gz")
        },
        PathBuf::from,
    )
}

#[expect(
    clippy::print_stderr,
    reason = "the file's size is printed with the timings"
)]
fn bench(c: &mut Criterion) {
    let path = path();
    let mut xml = Vec::new();
    let file = File::open(&path).unwrap();
    if path.extension().is_some_and(|e| e == "gz") {
        MultiGzDecoder::new(file).read_to_end(&mut xml).unwrap();
    } else {
        BufReader::new(file).read_to_end(&mut xml).unwrap();
    }

    let start = Instant::now();
    let summary = summarize(OscReader::new(xml.as_slice()));
    let secs = start.elapsed().as_secs_f64();
    #[allow(clippy::cast_precision_loss, reason = "for display")]
    let (mb, changes) = (xml.len() as f64 / 1e6, summary.changes as f64);
    eprintln!(
        "{}: {mb:.1} MB of XML, {summary:?}; one pass from memory took {secs:.3} s: {:.0} MB/s, {:.2} M changes/s",
        path.display(),
        mb / secs,
        changes / secs / 1e6
    );

    let mut group = c.benchmark_group("osc");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(xml.len() as u64));
    group.bench_function("xml in memory", |b| {
        b.iter(|| assert_eq!(summarize(OscReader::new(xml.as_slice())), summary));
    });
    group.bench_function("file", |b| {
        b.iter(|| assert_eq!(summarize(OscReader::open(&path).unwrap()), summary));
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
