//! Regression tests for defects found by the adversarial audit (docs/audit.md).
use std::sync::Arc;
use std::time::Duration;
use vectordb::collection::{Collection, Config, Error};
use vectordb::distance::Metric;
use vectordb::server::Registry;
use vectordb::wal::SyncMode;

fn cfg(dim: usize) -> Config {
    Config { dim, metric: Metric::L2, m: 8, ef_construction: 32, seed: 3 }
}

/// Collection::stats takes inner.read() then wal.lock(); Collection::write takes
/// wal.lock() then inner.write(). Lock-order inversion => deadlock.
#[test]
fn stats_concurrent_with_upserts_does_not_deadlock() {
    let d = tempfile::tempdir().unwrap();
    let c = Arc::new(Collection::open(d.path(), cfg(4), SyncMode::Never).unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let c1 = c.clone();
    let tx1 = tx.clone();
    std::thread::spawn(move || {
        for i in 0..20_000u64 {
            c1.upsert(i % 100, &[i as f32, 1.0, 2.0, 3.0], None).unwrap();
        }
        tx1.send(()).unwrap();
    });
    let c2 = c.clone();
    std::thread::spawn(move || {
        for _ in 0..200_000 {
            std::hint::black_box(c2.stats());
        }
        tx.send(()).unwrap();
    });
    // About 22 s of work on an idle 4-vCPU machine (2026-10-16), so 30 s
    // failed under shared-machine load. A deadlock never finishes, so a
    // generous bound still detects it.
    for _ in 0..2 {
        rx.recv_timeout(Duration::from_secs(120)).expect("deadlock: stats() vs upsert() did not finish within 120s");
    }
}

/// A corrupted length field in a MID-log record is treated as a torn tail:
/// every later (acknowledged, fsynced) record is silently discarded and the
/// file is truncated, instead of failing the open as documented.
#[test]
fn mid_log_length_corruption_is_not_silent_loss() {
    let d = tempfile::tempdir().unwrap();
    {
        let c = Collection::open(d.path(), cfg(4), SyncMode::Always).unwrap();
        for i in 0..10u64 {
            c.upsert(i, &[i as f32, 0.0, 0.0, 0.0], None).unwrap();
        }
    }
    let p = d.path().join("wal.log");
    let mut b = std::fs::read(&p).unwrap();
    b[8 + 2] ^= 0x01; // first record's length += 65536 (well under MAX_RECORD, past EOF)
    std::fs::write(&p, &b).unwrap();
    match Collection::open(d.path(), cfg(4), SyncMode::Always) {
        Err(_) => {} // acceptable: corruption reported
        Ok(c) => panic!("open succeeded with {} of 10 acknowledged points; {} bytes silently truncated", c.stats().live, c.recovered.wal_bytes_truncated),
    }
}

/// train_pq with max_train = 0 (accepted by POST /train_pq) panics in k-means
/// (`gen_range(0..0)`) instead of returning a BadRequest.
#[test]
fn train_pq_max_train_zero_is_an_error_not_a_panic() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), cfg(4), SyncMode::Never).unwrap();
    for i in 0..10u64 {
        c.upsert(i, &[i as f32, 1.0, 2.0, 3.0], None).unwrap();
    }
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.train_pq(2, 0).map(|_| ())));
    assert!(matches!(res, Ok(Err(Error::BadRequest(_)))), "train_pq(m=2, max_train=0) panicked or succeeded");
}

/// Cosine: a finite, non-zero vector whose squared norm overflows f32 is
/// rejected as a "zero vector".
#[test]
fn cosine_accepts_large_finite_vectors() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), Config { metric: Metric::Cosine, ..cfg(2) }, SyncMode::Never).unwrap();
    c.upsert(1, &[1e20, 1e20], None).expect("finite non-zero vector rejected");
}

/// drop_collection removes the map entry, then remove_dir_all()s the directory
/// outside the registry lock. A concurrent create() of the same name in that
/// window re-opens the old directory, registers a live collection, and then has
/// its directory deleted underneath it: acknowledged writes go to unlinked
/// files and the collection is gone after restart.
#[test]
fn drop_racing_create_does_not_lose_a_live_collection() {
    let d = tempfile::tempdir().unwrap();
    let reg = Registry::open(d.path().to_path_buf(), SyncMode::Never).unwrap();
    for iter in 0..2000 {
        reg.create("x", cfg(4)).unwrap();
        let r1 = reg.clone();
        let r2 = reg.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let (b1, b2) = (barrier.clone(), barrier.clone());
        let t1 = std::thread::spawn(move || {
            b1.wait();
            let _ = r1.drop_collection("x");
        });
        let t2 = std::thread::spawn(move || {
            b2.wait();
            for _ in 0..50 {
                if r2.create("x", cfg(4)).is_ok() {
                    // only meaningful if this create happened after the drop removed the entry
                }
                std::hint::spin_loop();
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        if let Ok(c) = reg.get("x") {
            if !c.dir.join("config.json").exists() {
                c.upsert(1, &[1.0; 4], None).unwrap(); // acknowledged...
                panic!("iteration {iter}: collection 'x' is registered and accepts writes, but its directory was deleted");
            }
            let _ = reg.drop_collection("x");
        }
    }
}

/// Suspected issue S5, fixed with the audit: a batch with one invalid point
/// must not leave the valid points before it applied.
#[test]
fn batch_upsert_is_all_or_nothing_on_validation_errors() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), cfg(2), SyncMode::Never).unwrap();
    let pts = vec![(1, vec![1.0, 0.0], None), (2, vec![0.0, 1.0], None), (3, vec![f32::NAN, 0.0], None)];
    match c.upsert_batch(pts) {
        Err(Error::BadRequest(m)) => assert!(m.contains("point 2"), "{m}"),
        other => panic!("expected BadRequest, got {other:?}"),
    }
    assert_eq!(c.stats().live, 0);
    assert_eq!(c.stats().lsn, 0);
    c.upsert_batch(vec![(1, vec![1.0, 0.0], None), (2, vec![0.0, 1.0], None)]).unwrap();
    assert_eq!(c.stats().live, 2);
}

/// S8: vectors whose distances would overflow f32 are rejected for L2 / IP.
#[test]
fn huge_vectors_are_rejected_for_l2() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), cfg(2), SyncMode::Never).unwrap();
    assert!(c.upsert(1, &[1e30, 1e30], None).is_err());
    c.upsert(1, &[1e10, 1e10], None).unwrap();
}
