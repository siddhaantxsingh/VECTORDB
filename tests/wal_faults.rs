//! Fault injection on the WAL append path: an I/O error during the record
//! write (after a short write), during fsync, and during the rollback itself.
//! The invariant checked: every acknowledged write survives a reopen, a failed
//! write is never acknowledged, and recovery is consistent. Real kernel I/O
//! errors (EFBIG via RLIMIT_FSIZE) are in `tests/wal_efbig.rs`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vectordb::collection::{Collection, Config};
use vectordb::distance::Metric;
use vectordb::wal::{FaultPoint, SyncMode};

const DIM: usize = 8;

fn cfg() -> Config {
    Config { dim: DIM, metric: Metric::L2, m: 8, ef_construction: 32, seed: 1 }
}

fn v(i: u64) -> Vec<f32> {
    (0..DIM).map(|j| ((i * 31 + j as u64) % 17) as f32 - 8.0).collect()
}

fn open(d: &Path) -> Collection {
    Collection::open(d, cfg(), SyncMode::Always).unwrap()
}

fn wal_len(d: &Path) -> u64 {
    std::fs::metadata(d.join("wal.log")).unwrap().len()
}

/// Fails each listed point once while `armed` is set.
fn hook(points: &'static [FaultPoint], armed: Arc<AtomicBool>) -> vectordb::wal::FaultHook {
    Box::new(move |p| if armed.load(Ordering::SeqCst) && points.contains(&p) { Err(std::io::Error::other(format!("injected {p:?} failure"))) } else { Ok(()) })
}

fn present(c: &Collection, ids: impl Iterator<Item = u64>) -> BTreeSet<u64> {
    ids.filter(|&i| c.get(i).is_some()).collect()
}

/// Writes 0..20, fails one write of id 100 at `point`, writes 20..30, then
/// checks the state before and after a reopen.
fn rollback_case(points: &'static [FaultPoint]) {
    let d = tempfile::tempdir().unwrap();
    let mut acked = BTreeSet::new();
    {
        let c = open(d.path());
        for i in 0..20 {
            c.upsert(i, &v(i), None).unwrap();
            acked.insert(i);
        }
        let (lsn, bytes) = (c.stats().lsn, c.stats().wal_bytes);
        assert_eq!(wal_len(d.path()), bytes);
        let armed = Arc::new(AtomicBool::new(true));
        c.set_wal_fault_hook(Some(hook(points, armed.clone())));
        let err = c.upsert(100, &v(100), None).unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        // not applied, LSN not consumed, file cut back to the last record
        assert!(c.get(100).is_none());
        assert_eq!(c.stats().lsn, lsn);
        assert_eq!(c.stats().wal_bytes, bytes);
        assert_eq!(wal_len(d.path()), bytes, "fragment of the failed record left in the log");
        // the log keeps working after the error
        armed.store(false, Ordering::SeqCst);
        for i in 20..30 {
            c.upsert(i, &v(i), None).unwrap();
            acked.insert(i);
        }
        c.check().unwrap();
    }
    let c = open(d.path());
    assert_eq!(c.recovered.wal_bytes_truncated, 0, "nothing to repair");
    assert_eq!(c.recovered.wal_records_replayed, 30);
    assert_eq!(present(&c, 0..200), acked, "acknowledged writes lost or failed write resurrected");
    for &i in &acked {
        assert_eq!(c.get(i).unwrap().0, v(i));
    }
    assert_eq!(c.stats().lsn, 30);
    c.check().unwrap();
}

#[test]
fn fsync_failure_rolls_back_the_record() {
    rollback_case(&[FaultPoint::Sync]);
}

#[test]
fn short_write_failure_rolls_back_the_fragment() {
    rollback_case(&[FaultPoint::Write]);
}

/// fsync fails and the rollback fails too. The log refuses further writes
/// until a restart. The complete but unacknowledged record is still in the
/// file, so it may reappear after the restart: the client got an error, so
/// either outcome is allowed. Acknowledged writes are never lost.
#[test]
fn failed_rollback_after_fsync_error_poisons_until_restart() {
    let d = tempfile::tempdir().unwrap();
    {
        let c = open(d.path());
        for i in 0..20 {
            c.upsert(i, &v(i), None).unwrap();
        }
        let armed = Arc::new(AtomicBool::new(true));
        c.set_wal_fault_hook(Some(hook(&[FaultPoint::Sync, FaultPoint::Rollback], armed.clone())));
        assert!(c.upsert(100, &v(100), None).is_err());
        armed.store(false, Ordering::SeqCst);
        let err = c.upsert(101, &v(101), None).unwrap_err();
        assert!(err.to_string().contains("unusable"), "{err}");
        assert!(c.get(100).is_none() && c.get(101).is_none());
    }
    let c = open(d.path());
    let got = present(&c, 0..200);
    let base: BTreeSet<u64> = (0..20).collect();
    assert!(got == base || got == base.iter().copied().chain([100]).collect(), "{got:?}");
    eprintln!("after restart: unacknowledged record 100 present = {}", got.contains(&100));
    assert!(!got.contains(&101));
    c.upsert(101, &v(101), None).unwrap();
    c.check().unwrap();
}

/// A short write whose rollback fails leaves a torn fragment. Recovery cuts
/// it off; the failed record does not reappear.
#[test]
fn failed_rollback_after_short_write_is_repaired_on_restart() {
    let d = tempfile::tempdir().unwrap();
    let bytes;
    {
        let c = open(d.path());
        for i in 0..20 {
            c.upsert(i, &v(i), None).unwrap();
        }
        bytes = c.stats().wal_bytes;
        let armed = Arc::new(AtomicBool::new(true));
        c.set_wal_fault_hook(Some(hook(&[FaultPoint::Write, FaultPoint::Rollback], armed)));
        assert!(c.upsert(100, &v(100), None).is_err());
        assert!(wal_len(d.path()) > bytes, "the injected short write should leave a fragment");
        assert!(c.upsert(101, &v(101), None).is_err());
    }
    let c = open(d.path());
    assert!(c.recovered.wal_bytes_truncated > 0);
    assert_eq!(wal_len(d.path()), bytes);
    assert_eq!(present(&c, 0..200), (0..20).collect());
    c.check().unwrap();
}
