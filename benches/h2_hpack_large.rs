//! Large dynamic-table HPACK benchmarks.
//!
//! Pre-populates the encoder's dynamic table with `N` distinct entries, then
//! re-encodes the same `N` headers (all cache hits) so the timed path runs
//! `Table::find` against a large table. This isolates lookup cost: the linear
//! scan (baseline) scales with `N`, while the hash map (optimized) stays O(1).
//! A crossover around small `N` is expected — tiny tables favour the scan's
//! early exits, large tables favour the hash map.

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use vibeio_http::hpack::{Encoder, Header};

fn populated_encoder(n: usize) -> (Encoder, Vec<Header>) {
    let mut encoder = Encoder::new(1 << 20);
    let headers: Vec<Header> = (0..n)
        .map(|i| {
            Header::new(
                Bytes::copy_from_slice(format!("x{i}").as_bytes()),
                Bytes::copy_from_slice(format!("v{i}").as_bytes()),
            )
        })
        .collect();
    let mut out = Vec::new();
    encoder.encode(&headers, &mut out);
    (encoder, headers)
}

fn bench_large_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("hpack_large_encode");
    for &n in &[32usize, 64, 128, 256, 512] {
        let (mut encoder, headers) = populated_encoder(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut out = Vec::new();
                encoder.encode(&headers, &mut out);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_large_encode);
criterion_main!(benches);
