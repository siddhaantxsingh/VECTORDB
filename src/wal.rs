//! Write-ahead log.
//!
//! File layout: 8-byte magic `VDBWAL1\n`, then records
//! `[len: u32 LE][crc32(payload): u32 LE][payload]`.
//! Payload: `lsn u64 | op u8 | ext id u64 | (insert) dim u32, dim × f32, meta_len u32, meta JSON`.
//!
//! Recovery rules (tested by truncating and corrupting the log at every byte):
//! * a record whose header or payload extends past end-of-file is a torn
//!   write at the tail: the log is truncated to the last complete record;
//! * the LAST record with a CRC mismatch is also treated as a torn tail;
//! * an all-zero tail (file extended, data never written) is a torn tail;
//! * a CRC mismatch on a record that is followed by more data is corruption:
//!   opening fails rather than silently dropping acknowledged writes.

use crate::filter::Metadata;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"VDBWAL1\n";
pub const MAX_RECORD: u32 = 256 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// fsync after every write (durable across power loss, slower)
    Always,
    /// leave flushing to the OS (durable across process crash only)
    Never,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Upsert { ext: u64, vector: Vec<f32>, meta: Option<Metadata> },
    Delete { ext: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub lsn: u64,
    pub op: Op,
}

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt write-ahead log at offset {0}: {1}")]
    Corrupt(u64, String),
}

pub fn encode(r: &Record) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&r.lsn.to_le_bytes());
    match &r.op {
        Op::Upsert { ext, vector, meta } => {
            p.push(1);
            p.extend_from_slice(&ext.to_le_bytes());
            p.extend_from_slice(&(vector.len() as u32).to_le_bytes());
            for x in vector {
                p.extend_from_slice(&x.to_le_bytes());
            }
            let m = meta.as_ref().map(|m| serde_json::to_vec(m).expect("metadata serialises")).unwrap_or_default();
            p.extend_from_slice(&(m.len() as u32).to_le_bytes());
            p.extend_from_slice(&m);
        }
        Op::Delete { ext } => {
            p.push(2);
            p.extend_from_slice(&ext.to_le_bytes());
        }
    }
    let mut out = Vec::with_capacity(p.len() + 8);
    out.extend_from_slice(&(p.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(&p).to_le_bytes());
    out.extend_from_slice(&p);
    out
}

fn decode_payload(p: &[u8]) -> Result<Record, String> {
    let mut c = p;
    let take = |c: &mut &[u8], n: usize| -> Result<Vec<u8>, String> {
        if c.len() < n {
            return Err("payload too short".into());
        }
        let (a, b) = c.split_at(n);
        *c = b;
        Ok(a.to_vec())
    };
    let lsn = u64::from_le_bytes(take(&mut c, 8)?.try_into().unwrap());
    let op = take(&mut c, 1)?[0];
    let ext = u64::from_le_bytes(take(&mut c, 8)?.try_into().unwrap());
    let op = match op {
        1 => {
            let dim = u32::from_le_bytes(take(&mut c, 4)?.try_into().unwrap()) as usize;
            if dim > (1 << 16) {
                return Err("dimension too large".into());
            }
            let raw = take(&mut c, dim * 4)?;
            let vector = raw.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
            let ml = u32::from_le_bytes(take(&mut c, 4)?.try_into().unwrap()) as usize;
            let mb = take(&mut c, ml)?;
            let meta = if ml == 0 { None } else { Some(serde_json::from_slice(&mb).map_err(|e| e.to_string())?) };
            Op::Upsert { ext, vector, meta }
        }
        2 => Op::Delete { ext },
        x => return Err(format!("unknown op {x}")),
    };
    if !c.is_empty() {
        return Err("trailing bytes in payload".into());
    }
    Ok(Record { lsn, op })
}

/// First offset `>= from` at which a complete, checksummed, decodable record
/// starts. Only used on the (rare) torn-tail path during recovery.
fn later_valid_record(data: &[u8], from: usize) -> Option<usize> {
    const MIN_PAYLOAD: usize = 8 + 1 + 8; // lsn, op, ext id
    (from..data.len().saturating_sub(8 + MIN_PAYLOAD - 1)).find(|&p| {
        let len = u32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as usize;
        let end = p + 8 + len;
        len >= MIN_PAYLOAD
            && len <= MAX_RECORD as usize
            && end <= data.len()
            && crc32fast::hash(&data[p + 8..end]) == u32::from_le_bytes(data[p + 4..p + 8].try_into().unwrap())
            && decode_payload(&data[p + 8..end]).is_ok()
    })
}

/// Points in [`Wal::append`] where a test can inject an I/O error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPoint {
    /// the record write: the first half of the record is written, then the
    /// error is returned (a short write followed by an error)
    Write,
    /// the fsync after a complete write (`SyncMode::Always` only)
    Sync,
    /// the truncate that rolls a failed append back
    Rollback,
}

/// A fault-injection hook: returning `Err` makes that step fail.
pub type FaultHook = Box<dyn FnMut(FaultPoint) -> std::io::Result<()> + Send>;

pub struct Wal {
    file: File,
    sync: SyncMode,
    pub bytes: u64,
    failed: bool,
    faults: Option<FaultHook>,
}

impl Wal {
    /// Opens (creating if needed) and returns all valid records, repairing a
    /// torn tail. Fails on mid-log corruption.
    pub fn open(path: &Path, sync: SyncMode) -> Result<(Wal, Vec<Record>, u64), WalError> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        let mut repaired = 0u64;
        if data.len() < MAGIC.len() {
            // empty or torn header: (re)initialise
            if !data.is_empty() && !MAGIC.starts_with(&data) {
                return Err(WalError::Corrupt(0, "bad magic".into()));
            }
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(MAGIC)?;
            file.sync_all()?;
            return Ok((Wal { file, sync, bytes: MAGIC.len() as u64, failed: false, faults: None }, Vec::new(), data.len() as u64));
        }
        if &data[..8] != MAGIC {
            return Err(WalError::Corrupt(0, "bad magic".into()));
        }
        let mut off = 8usize;
        let mut recs = Vec::new();
        while off < data.len() {
            if data[off..].iter().all(|&b| b == 0) {
                break; // zero-filled tail (file extended but data never written)
            }
            if off + 8 > data.len() {
                break; // torn header
            }
            let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
            let crc = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap());
            if len > MAX_RECORD {
                return Err(WalError::Corrupt(off as u64, format!("record length {len}")));
            }
            let end = off + 8 + len as usize;
            if end > data.len() {
                // Either a torn final write, or a corrupted length field in the
                // middle of the log. Appends are whole records, so a torn write
                // is never followed by a complete record: if one exists later,
                // this is corruption and acknowledged records would be lost.
                if let Some(p) = later_valid_record(&data, off + 1) {
                    return Err(WalError::Corrupt(off as u64, format!("record length {len} runs past end of file, but a valid record follows at offset {p}")));
                }
                break; // torn payload
            }
            let payload = &data[off + 8..end];
            if crc32fast::hash(payload) != crc {
                if end == data.len() {
                    break; // last record torn
                }
                return Err(WalError::Corrupt(off as u64, "checksum mismatch".into()));
            }
            match decode_payload(payload) {
                Ok(r) => recs.push(r),
                Err(e) => return Err(WalError::Corrupt(off as u64, e)),
            }
            off = end;
        }
        if off < data.len() {
            repaired = (data.len() - off) as u64;
            file.set_len(off as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::Start(off as u64))?;
        Ok((Wal { file, sync, bytes: off as u64, failed: false, faults: None }, recs, repaired))
    }

    /// Appends one record. On any failure (short write, ENOSPC, failed
    /// fsync) the file is cut back to the last complete record, so a failed
    /// (unacknowledged) write can neither leave a fragment that later appends
    /// land behind nor be replayed after a restart. If even the rollback fails
    /// the log refuses all further writes.
    pub fn append(&mut self, r: &Record) -> Result<(), WalError> {
        if self.failed {
            return Err(WalError::Io(std::io::Error::other("write-ahead log is unusable after an unrecoverable I/O error; restart to recover")));
        }
        let buf = encode(r);
        let res = self.write_record(&buf).and_then(|_| {
            if self.sync == SyncMode::Always {
                self.fault(FaultPoint::Sync)?;
                self.file.sync_data()
            } else {
                Ok(())
            }
        });
        if let Err(e) = res {
            let rollback = self.fault(FaultPoint::Rollback).and_then(|_| self.file.set_len(self.bytes)).and_then(|_| self.file.seek(SeekFrom::Start(self.bytes)).map(|_| ()));
            if rollback.is_err() {
                self.failed = true;
            }
            return Err(e.into());
        }
        self.bytes += buf.len() as u64;
        Ok(())
    }

    fn fault(&mut self, p: FaultPoint) -> std::io::Result<()> {
        match self.faults.as_mut() {
            Some(f) => f(p),
            None => Ok(()),
        }
    }

    fn write_record(&mut self, buf: &[u8]) -> std::io::Result<()> {
        if let Err(e) = self.fault(FaultPoint::Write) {
            self.file.write_all(&buf[..buf.len() / 2])?;
            return Err(e);
        }
        self.file.write_all(buf)
    }

    /// Test support: install (or with `None` remove) a fault-injection hook.
    #[doc(hidden)]
    pub fn set_fault_hook(&mut self, hook: Option<FaultHook>) {
        self.faults = hook;
    }

    /// True after a failed rollback: the log refuses all writes until reopened.
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Drops all records (after a snapshot made them redundant).
    pub fn reset(&mut self) -> Result<(), WalError> {
        self.file.set_len(MAGIC.len() as u64)?;
        self.file.seek(SeekFrom::Start(MAGIC.len() as u64))?;
        self.file.sync_all()?;
        self.bytes = MAGIC.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recs() -> Vec<Record> {
        let mut m = Metadata::new();
        m.insert("k".into(), crate::filter::Value::Str("v".into()));
        vec![
            Record { lsn: 1, op: Op::Upsert { ext: 7, vector: vec![1.0, 2.0, 3.0], meta: Some(m) } },
            Record { lsn: 2, op: Op::Delete { ext: 7 } },
            Record { lsn: 3, op: Op::Upsert { ext: 9, vector: vec![0.5; 3], meta: None } },
        ]
    }

    #[test]
    fn round_trip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("wal");
        {
            let (mut w, r, _) = Wal::open(&p, SyncMode::Always).unwrap();
            assert!(r.is_empty());
            for x in recs() {
                w.append(&x).unwrap();
            }
        }
        let (_, r, rep) = Wal::open(&p, SyncMode::Always).unwrap();
        assert_eq!(r, recs());
        assert_eq!(rep, 0);
    }

    #[test]
    fn truncation_at_every_byte_recovers_a_prefix() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("wal");
        {
            let (mut w, _, _) = Wal::open(&p, SyncMode::Never).unwrap();
            for x in recs() {
                w.append(&x).unwrap();
            }
        }
        let full = std::fs::read(&p).unwrap();
        let bounds: Vec<usize> = {
            let mut b = vec![8];
            for x in recs() {
                b.push(b.last().unwrap() + encode(&x).len());
            }
            b
        };
        for cut in 0..=full.len() {
            let q = d.path().join(format!("w{cut}"));
            std::fs::write(&q, &full[..cut]).unwrap();
            let (_, r, _) = Wal::open(&q, SyncMode::Never).unwrap_or_else(|e| panic!("cut {cut}: {e}"));
            let complete = bounds.iter().filter(|&&b| b <= cut).count().saturating_sub(1);
            assert_eq!(r, recs()[..complete].to_vec(), "cut {cut}");
            // the repaired file must accept appends and read back cleanly
            let (mut w, _, _) = Wal::open(&q, SyncMode::Never).unwrap();
            w.append(&Record { lsn: 99, op: Op::Delete { ext: 1 } }).unwrap();
            let (_, r2, _) = Wal::open(&q, SyncMode::Never).unwrap();
            assert_eq!(r2.len(), complete + 1, "cut {cut}");
        }
    }

    #[test]
    fn zero_filled_tail_is_repaired() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("wal");
        {
            let (mut w, _, _) = Wal::open(&p, SyncMode::Never).unwrap();
            w.append(&recs()[0]).unwrap();
        }
        let mut b = std::fs::read(&p).unwrap();
        b.extend(std::iter::repeat_n(0u8, 4096));
        std::fs::write(&p, &b).unwrap();
        let (_, r, rep) = Wal::open(&p, SyncMode::Never).unwrap();
        assert_eq!(r, recs()[..1].to_vec());
        assert_eq!(rep, 4096);
    }

    #[test]
    fn mid_log_corruption_is_an_error_not_silent_loss() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("wal");
        {
            let (mut w, _, _) = Wal::open(&p, SyncMode::Never).unwrap();
            for x in recs() {
                w.append(&x).unwrap();
            }
        }
        let mut b = std::fs::read(&p).unwrap();
        b[8 + 8 + 3] ^= 0xFF; // inside the first record's payload
        std::fs::write(&p, &b).unwrap();
        assert!(matches!(Wal::open(&p, SyncMode::Never), Err(WalError::Corrupt(8, _))));
    }
}
