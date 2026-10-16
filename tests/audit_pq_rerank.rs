//! Regression tests for defects found by the adversarial audit (docs/audit.md).
//! Separate file: before the fix this test aborts the whole process (allocation failure).
use vectordb::collection::{Collection, Config, Error, Mode, SearchRequest};
use vectordb::distance::Metric;
use vectordb::wal::SyncMode;

fn cfg(dim: usize) -> Config {
    Config { dim, metric: Metric::L2, m: 8, ef_construction: 32, seed: 3 }
}

/// PQ search: `k * rerank` is unbounded and used for BinaryHeap::with_capacity.
/// A single request aborts the whole process (allocation failure) or panics
/// on multiply overflow. Reachable via REST (rerank: usize) and gRPC.
#[test]
fn pq_search_huge_rerank_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), cfg(4), SyncMode::Never).unwrap();
    for i in 0..50u64 {
        c.upsert(i, &[i as f32, 1.0, (i % 7) as f32, 2.0], None).unwrap();
    }
    c.train_pq(2, 1000).unwrap();
    let r = SearchRequest { vector: vec![1.0; 4], k: 10, ef: None, filter: None, mode: Mode::Pq, rerank: Some(1usize << 40) };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.search(&r)));
    assert!(matches!(res, Ok(Err(Error::BadRequest(_)))), "expected BadRequest");
}
