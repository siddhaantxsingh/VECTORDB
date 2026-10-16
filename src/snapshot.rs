//! Snapshot files: the complete in-memory state (vectors, ids, tombstones,
//! metadata, HNSW graph, PQ codebooks and codes) at a given LSN.
//!
//! Layout: `VDBSNAP1` magic, format version, body, then CRC32 of everything
//! before it. Writing goes to `snapshot.tmp`, is fsynced, renamed over
//! `snapshot.vdb`, and the directory is fsynced, so a crash leaves either the
//! old or the new snapshot, never a partial one. A checksum failure on load
//! is an error (no silent fallback to an empty collection).

use crate::distance::Metric;
use crate::hnsw::{Hnsw, HnswParams};
use crate::pq::Pq;
use crate::store::VectorStore;
use std::fs::File;
use std::io::Write;
use std::path::Path;

const MAGIC: &[u8; 8] = b"VDBSNAP1";
const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum SnapError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt snapshot: {0}")]
    Corrupt(String),
}

struct W(Vec<u8>);
impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v)
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes())
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes())
    }
    fn f32s(&mut self, v: &[f32]) {
        for x in v {
            self.0.extend_from_slice(&x.to_le_bytes());
        }
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u64(b.len() as u64);
        self.0.extend_from_slice(b);
    }
}

struct R<'a>(&'a [u8]);
impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapError> {
        if self.0.len() < n {
            return Err(SnapError::Corrupt("truncated".into()));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, SnapError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, SnapError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, SnapError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn len(&mut self, max: usize) -> Result<usize, SnapError> {
        let n = self.u64()? as usize;
        if n > max {
            return Err(SnapError::Corrupt(format!("length {n} exceeds {max}")));
        }
        Ok(n)
    }
    fn f32s(&mut self, n: usize) -> Result<Vec<f32>, SnapError> {
        Ok(self.take(n * 4)?.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
    }
    fn bytes(&mut self) -> Result<&'a [u8], SnapError> {
        let n = self.len(self.0.len())?;
        self.take(n)
    }
}

pub struct State {
    pub lsn: u64,
    pub store: VectorStore,
    pub hnsw: Hnsw,
    pub pq: Option<Pq>,
}

pub fn encode(s: &State) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(16 + s.store.data.len() * 4));
    w.0.extend_from_slice(MAGIC);
    w.u32(VERSION);
    w.u64(s.lsn);
    w.u32(s.store.dim as u32);
    w.u8(s.store.metric.as_u8());
    let p = s.hnsw.params;
    w.u32(p.m as u32);
    w.u32(p.ef_construction as u32);
    w.u64(p.seed);
    let n = s.store.len();
    w.u64(n as u64);
    w.f32s(&s.store.data);
    for &e in &s.store.ext_ids {
        w.u64(e);
    }
    for &d in &s.store.deleted {
        w.u8(d as u8);
    }
    for m in &s.store.metadata {
        match m {
            Some(m) => {
                w.u8(1);
                w.bytes(&serde_json::to_vec(m).expect("metadata serialises"));
            }
            None => w.u8(0),
        }
    }
    w.0.extend_from_slice(&s.hnsw.levels);
    for per in &s.hnsw.links {
        for list in per {
            w.u32(list.len() as u32);
            for &x in list {
                w.u32(x);
            }
        }
    }
    w.u32(s.hnsw.entry.unwrap_or(u32::MAX));
    w.u32(s.hnsw.max_level as u32);
    match &s.pq {
        None => w.u8(0),
        Some(pq) => {
            w.u8(1);
            w.u32(pq.m as u32);
            w.u32(pq.ksub as u32);
            w.u32(pq.dsub as u32);
            w.f32s(&pq.centroids);
            w.bytes(&pq.codes);
        }
    }
    let crc = crc32fast::hash(&w.0);
    w.u32(crc);
    w.0
}

pub fn decode(buf: &[u8]) -> Result<State, SnapError> {
    if buf.len() < 12 || &buf[..8] != MAGIC {
        return Err(SnapError::Corrupt("bad magic".into()));
    }
    let (body, tail) = buf.split_at(buf.len() - 4);
    if crc32fast::hash(body) != u32::from_le_bytes(tail.try_into().unwrap()) {
        return Err(SnapError::Corrupt("checksum mismatch".into()));
    }
    let mut r = R(&body[8..]);
    if r.u32()? != VERSION {
        return Err(SnapError::Corrupt("unsupported version".into()));
    }
    let lsn = r.u64()?;
    let dim = r.u32()? as usize;
    let metric = Metric::from_u8(r.u8()?).ok_or_else(|| SnapError::Corrupt("metric".into()))?;
    let params = HnswParams { m: r.u32()? as usize, ef_construction: r.u32()? as usize, seed: r.u64()? };
    if dim == 0 || dim > 1 << 16 || params.m < 2 {
        return Err(SnapError::Corrupt("parameters".into()));
    }
    let n = r.len(body.len())?;
    let mut store = VectorStore::new(dim, metric);
    store.data = r.f32s(n.checked_mul(dim).ok_or_else(|| SnapError::Corrupt("size".into()))?)?;
    store.ext_ids = (0..n).map(|_| r.u64()).collect::<Result<_, _>>()?;
    store.deleted = (0..n).map(|_| r.u8().map(|b| b != 0)).collect::<Result<_, _>>()?;
    for _ in 0..n {
        store.metadata.push(match r.u8()? {
            0 => None,
            _ => Some(serde_json::from_slice(r.bytes()?).map_err(|e| SnapError::Corrupt(e.to_string()))?),
        });
    }
    for (i, &e) in store.ext_ids.iter().enumerate() {
        if !store.deleted[i] {
            if store.by_ext.insert(e, i as u32).is_some() {
                return Err(SnapError::Corrupt(format!("external id {e} live twice")));
            }
            store.live += 1;
        }
    }
    let levels = r.take(n)?.to_vec();
    let mut links = Vec::with_capacity(n);
    for &l in &levels {
        let mut per = Vec::with_capacity(l as usize + 1);
        for _ in 0..=l {
            let c = r.u32()? as usize;
            if c > 4 * params.m + 4 {
                return Err(SnapError::Corrupt("link list too long".into()));
            }
            let list: Vec<u32> = (0..c).map(|_| r.u32()).collect::<Result<_, _>>()?;
            if list.iter().any(|&x| x as usize >= n) {
                return Err(SnapError::Corrupt("link out of range".into()));
            }
            per.push(list);
        }
        links.push(per);
    }
    let entry = match r.u32()? {
        u32::MAX => None,
        e if (e as usize) < n => Some(e),
        _ => return Err(SnapError::Corrupt("entry point".into())),
    };
    let max_level = r.u32()? as usize;
    let hnsw = Hnsw::from_parts(params, levels, links, entry, max_level, params.seed ^ lsn);
    hnsw.check_invariants().map_err(SnapError::Corrupt)?;
    let pq = match r.u8()? {
        0 => None,
        _ => {
            let (m, ksub, dsub) = (r.u32()? as usize, r.u32()? as usize, r.u32()? as usize);
            if m * dsub != dim || ksub == 0 || ksub > 256 {
                return Err(SnapError::Corrupt("pq parameters".into()));
            }
            let centroids = r.f32s(m * ksub * dsub)?;
            let codes = r.bytes()?.to_vec();
            if codes.len() != n * m || codes.iter().any(|&c| c as usize >= ksub) {
                return Err(SnapError::Corrupt("pq codes".into()));
            }
            Some(Pq { m, ksub, dsub, metric, centroids, codes })
        }
    };
    if !r.0.is_empty() {
        return Err(SnapError::Corrupt("trailing bytes".into()));
    }
    Ok(State { lsn, store, hnsw, pq })
}

pub fn write_atomic(dir: &Path, s: &State) -> Result<(), SnapError> {
    let tmp = dir.join("snapshot.tmp");
    let fin = dir.join("snapshot.vdb");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&encode(s))?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &fin)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

pub fn read(dir: &Path) -> Result<Option<State>, SnapError> {
    let p = dir.join("snapshot.vdb");
    match std::fs::read(&p) {
        Ok(b) => decode(&b).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
