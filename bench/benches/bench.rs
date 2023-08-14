use std::fs;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion};
use osmnodecache::{CacheStore, DenseFileCacheOpts};

fn bench_crate(c: &mut Criterion) {
    c.bench_function("bench", |b| {
        let test_file = "./dense_file_perf.dat";
        let _ = fs::remove_file(test_file);
        let fc = DenseFileCacheOpts::new(PathBuf::from(test_file))
            .page_size(1024 * 1024)
            .open()
            .unwrap();

        let mut cache = fc.get_accessor();
        b.iter(|| {
            for v in 0..1000 {
                cache.set(v, v as u64);
            }
        });
        let _ = fs::remove_file(test_file);
    });
}

criterion_group!(benches, bench_crate);
criterion_main!(benches);
