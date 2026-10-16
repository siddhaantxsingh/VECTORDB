//! A real kernel I/O error on the WAL append path, without root: the
//! process's RLIMIT_FSIZE is lowered so that writing past the limit fails
//! with EFBIG (SIGXFSZ is ignored). The write that crosses the limit is a
//! genuine short write followed by an error from the kernel.
//!
//! This file holds a single test because the limit applies to the whole
//! process.

use std::collections::BTreeSet;
use vectordb::collection::{Collection, Config};
use vectordb::distance::Metric;
use vectordb::wal::SyncMode;

const DIM: usize = 16;

fn v(i: u64) -> Vec<f32> {
    (0..DIM).map(|j| ((i * 7 + j as u64) % 13) as f32).collect()
}

fn set_fsize(limit: libc::rlim_t) {
    let r = libc::rlimit { rlim_cur: limit, rlim_max: libc::RLIM_INFINITY };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &r) }, 0, "setrlimit");
}

#[test]
fn efbig_during_append_loses_no_acknowledged_write() {
    unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    let d = tempfile::tempdir().unwrap();
    let cfg = Config { dim: DIM, metric: Metric::L2, m: 8, ef_construction: 32, seed: 1 };
    let mut acked = BTreeSet::new();
    let failed_id;
    {
        let c = Collection::open(d.path(), cfg.clone(), SyncMode::Always).unwrap();
        for i in 0..10 {
            c.upsert(i, &v(i), None).unwrap();
            acked.insert(i);
        }
        let per_record = c.stats().wal_bytes / 10; // roughly; records are equal-sized here
                                                   // room for 3.5 more records: the 4th write is cut short by the kernel
        let limit = c.stats().wal_bytes + per_record * 7 / 2;
        set_fsize(limit as libc::rlim_t);
        let mut i = 10;
        let err = loop {
            match c.upsert(i, &v(i), None) {
                Ok(_) => {
                    acked.insert(i);
                    i += 1;
                    assert!(i < 100, "the limit was never hit");
                }
                Err(e) => break e,
            }
        };
        failed_id = i;
        set_fsize(libc::RLIM_INFINITY);
        let msg = err.to_string();
        eprintln!("append of id {failed_id} failed with: {msg}");
        assert!(msg.contains("File too large") || msg.contains("os error 27"), "{msg}");
        assert_eq!(acked.len(), 13, "3 appends fit under the limit");
        let on_disk = std::fs::metadata(d.path().join("wal.log")).unwrap().len();
        assert_eq!(on_disk, c.stats().wal_bytes, "the partial record was not rolled back");
        assert!(c.get(failed_id).is_none());
        // the log is usable again once space is available
        c.upsert(1000, &v(1000), None).unwrap();
        acked.insert(1000);
    }
    let c = Collection::open(d.path(), cfg, SyncMode::Always).unwrap();
    assert_eq!(c.recovered.wal_bytes_truncated, 0);
    let present: BTreeSet<u64> = (0..1001).filter(|&i| c.get(i).is_some()).collect();
    assert_eq!(present, acked);
    assert!(!present.contains(&failed_id));
    c.check().unwrap();
}
