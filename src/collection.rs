//! A persistent collection: vector store + HNSW (+ optional PQ), a WAL for
//! durability and snapshots for fast recovery.
//!
//! Write path (upsert/delete): under the WAL mutex, assign the next LSN,
//! append the record (fsync in `SyncMode::Always`), THEN apply it to the
//! in-memory index under the index write lock. A write is acknowledged only
//! after its WAL append returned, so with `SyncMode::Always` an acknowledged
//! write survives a crash at any point. Readers (searches) take the index
//! read lock and run concurrently with each other; they block only while a
//! write is being applied in memory (not during the fsync).
//!
//! Recovery: load the snapshot (if any), then replay WAL records with
//! LSN > snapshot LSN. A torn WAL tail is truncated; mid-log corruption or a
//! corrupt snapshot fails the open.

use crate::distance::Metric;
use crate::filter::{Filter, Metadata};
use crate::hnsw::{exact_search, Hnsw, HnswParams};
use crate::pq::Pq;
use crate::snapshot::{self, State};
use crate::store::{StoreError, VectorStore};
use crate::wal::{Op, Record, SyncMode, Wal, WalError};
use parking_lot::{Mutex, RwLock};
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Invalid(#[from] StoreError),
    #[error("{0}")]
    BadRequest(String),
    #[error(transparent)]
    Wal(#[from] WalError),
    #[error(transparent)]
    Snapshot(#[from] snapshot::SnapError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("collection config mismatch: {0}")]
    Config(String),
    #[error("collection directory {0} is already open (LOCK held by another handle or process)")]
    Locked(PathBuf),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub dim: usize,
    pub metric: Metric,
    pub m: usize,
    pub ef_construction: usize,
    pub seed: u64,
}

impl Config {
    pub fn new(dim: usize, metric: Metric) -> Self {
        Config { dim, metric, m: 16, ef_construction: 200, seed: 42 }
    }
    fn params(&self) -> HnswParams {
        HnswParams { m: self.m, ef_construction: self.ef_construction, seed: self.seed }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Hnsw,
    Exact,
    Pq,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub vector: Vec<f32>,
    pub k: usize,
    #[serde(default)]
    pub ef: Option<usize>,
    #[serde(default)]
    pub filter: Option<Filter>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub rerank: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Hit {
    pub id: u64,
    pub distance: f32,
    pub metadata: Option<Metadata>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
    /// how the query was executed ("hnsw", "hnsw-filtered ef=..", "exact-filter-fallback", "exact", "pq")
    pub plan: String,
    pub micros: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stats {
    pub dim: usize,
    pub metric: Metric,
    pub live: usize,
    pub slots: usize,
    pub deleted_fraction: f64,
    pub lsn: u64,
    pub wal_bytes: u64,
    pub hnsw_max_level: usize,
    pub pq: Option<(usize, usize)>,
    pub vector_bytes: usize,
    pub graph_bytes: usize,
    pub pq_bytes: usize,
    pub queries: u64,
    pub writes: u64,
}

struct Inner {
    store: VectorStore,
    hnsw: Hnsw,
    pq: Option<Pq>,
    lsn: u64,
}

pub struct Collection {
    pub dir: PathBuf,
    pub config: Config,
    inner: RwLock<Inner>,
    wal: Mutex<Wal>,
    pub queries: AtomicU64,
    pub writes: AtomicU64,
    pub recovered: RecoveryReport,
    /// Exclusive advisory lock on `<dir>/LOCK`, held for the handle's lifetime
    /// so two writers can never append to the same WAL.
    _lock: std::fs::File,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RecoveryReport {
    pub snapshot_lsn: Option<u64>,
    pub wal_records_replayed: usize,
    pub wal_bytes_truncated: u64,
}

pub const MAX_K: usize = 10_000;
pub const MAX_EF: usize = 10_000;
/// Upper bounds for PQ re-ranking: `rerank` factor and `k * rerank` candidates.
pub const MAX_RERANK: usize = 1_000;
pub const MAX_RERANK_CANDIDATES: usize = 100_000;
/// Below this estimated filter selectivity, filtered queries scan exactly.
pub const EXACT_FALLBACK_SELECTIVITY: f64 = 0.02;

impl Collection {
    /// Opens `dir`, creating the collection with `config` if it is new. An
    /// existing collection's stored config must match `config`.
    pub fn open(dir: &Path, config: Config, sync: SyncMode) -> Result<Collection, Error> {
        if config.dim == 0 || config.dim > 1 << 16 {
            return Err(Error::Config("dimension must be 1..65536".into()));
        }
        if config.m < 2 || config.m > 128 || config.ef_construction < 1 || config.ef_construction > 4096 {
            return Err(Error::Config("m must be 2..128 and ef_construction 1..4096".into()));
        }
        std::fs::create_dir_all(dir)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("LOCK"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(Error::Locked(dir.to_path_buf())),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let cfg_path = dir.join("config.json");
        match std::fs::read(&cfg_path) {
            Ok(b) => {
                let stored: Config = serde_json::from_slice(&b).map_err(|e| Error::Config(e.to_string()))?;
                if stored != config {
                    return Err(Error::Config(format!("stored {stored:?}, requested {config:?}")));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let tmp = dir.join("config.tmp");
                std::fs::write(&tmp, serde_json::to_vec_pretty(&config).unwrap())?;
                std::fs::File::open(&tmp)?.sync_all()?;
                std::fs::rename(&tmp, &cfg_path)?;
                std::fs::File::open(dir)?.sync_all()?;
                // make the new collection directory's own entry durable too
                if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::File::open(parent)?.sync_all()?;
                }
            }
            Err(e) => return Err(e.into()),
        }
        let mut report = RecoveryReport::default();
        let (mut inner, snap_lsn) = match snapshot::read(dir)? {
            Some(s) => {
                if s.store.dim != config.dim || s.store.metric != config.metric || s.hnsw.params != config.params() {
                    return Err(Error::Config("snapshot does not match config".into()));
                }
                report.snapshot_lsn = Some(s.lsn);
                let l = s.lsn;
                (Inner { store: s.store, hnsw: s.hnsw, pq: s.pq, lsn: s.lsn }, l)
            }
            None => (Inner { store: VectorStore::new(config.dim, config.metric), hnsw: Hnsw::new(config.params()), pq: None, lsn: 0 }, 0),
        };
        let (wal, recs, truncated) = Wal::open(&dir.join("wal.log"), sync)?;
        report.wal_bytes_truncated = truncated;
        for r in recs {
            if r.lsn <= snap_lsn {
                continue;
            }
            apply(&mut inner, &r.op)?;
            inner.lsn = r.lsn;
            report.wal_records_replayed += 1;
        }
        Ok(Collection {
            dir: dir.to_path_buf(),
            config,
            inner: RwLock::new(inner),
            wal: Mutex::new(wal),
            queries: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            recovered: report,
            _lock: lock,
        })
    }

    fn write(&self, op: Op) -> Result<u64, Error> {
        let mut wal = self.wal.lock();
        let lsn = self.inner.read().lsn + 1;
        wal.append(&Record { lsn, op: op.clone() })?;
        let mut inner = self.inner.write();
        apply(&mut inner, &op)?;
        inner.lsn = lsn;
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(lsn)
    }

    /// Validates (and for cosine normalises) one point without writing it.
    fn prepare(&self, vector: &[f32], meta: &Option<Metadata>) -> Result<Vec<f32>, Error> {
        let v = self.inner.read().store.prepare(vector)?;
        if let Some(m) = meta {
            if serde_json::to_vec(m).map(|b| b.len()).unwrap_or(usize::MAX) > 64 * 1024 {
                return Err(Error::BadRequest("metadata larger than 64 KiB".into()));
            }
        }
        Ok(v)
    }

    /// Test support: install a WAL fault-injection hook (see `wal::FaultPoint`).
    #[doc(hidden)]
    pub fn set_wal_fault_hook(&self, hook: Option<crate::wal::FaultHook>) {
        self.wal.lock().set_fault_hook(hook);
    }

    pub fn upsert(&self, id: u64, vector: &[f32], meta: Option<Metadata>) -> Result<u64, Error> {
        // Validate BEFORE logging, so an invalid request never reaches the WAL.
        let v = self.prepare(vector, &meta)?;
        self.write(Op::Upsert { ext: id, vector: v, meta })
    }

    /// Upserts a batch. Every point is validated before any is written, so an
    /// invalid point rejects the whole batch with nothing applied; the error
    /// names the index of the offending point. (An I/O error part-way through
    /// can still leave a prefix applied; each applied point is durable.)
    /// Returns the LSN of the last write (the current LSN for an empty batch).
    pub fn upsert_batch(&self, points: Vec<(u64, Vec<f32>, Option<Metadata>)>) -> Result<u64, Error> {
        let mut ops = Vec::with_capacity(points.len());
        for (i, (id, v, meta)) in points.into_iter().enumerate() {
            let v = self.prepare(&v, &meta).map_err(|e| Error::BadRequest(format!("point {i} (id {id}): {e}")))?;
            ops.push(Op::Upsert { ext: id, vector: v, meta });
        }
        let mut lsn = self.inner.read().lsn;
        for op in ops {
            lsn = self.write(op)?;
        }
        Ok(lsn)
    }

    pub fn delete(&self, id: u64) -> Result<bool, Error> {
        if !self.inner.read().store.by_ext.contains_key(&id) {
            return Ok(false);
        }
        self.write(Op::Delete { ext: id })?;
        Ok(true)
    }

    pub fn get(&self, id: u64) -> Option<(Vec<f32>, Option<Metadata>)> {
        let inner = self.inner.read();
        let slot = *inner.store.by_ext.get(&id)?;
        Some((inner.store.vector(slot).to_vec(), inner.store.metadata[slot as usize].clone()))
    }

    pub fn search(&self, req: &SearchRequest) -> Result<SearchResponse, Error> {
        let t0 = Instant::now();
        if req.k == 0 || req.k > MAX_K {
            return Err(Error::BadRequest(format!("k must be 1..{MAX_K}")));
        }
        let ef = req.ef.unwrap_or(64.max(req.k));
        if ef > MAX_EF {
            return Err(Error::BadRequest(format!("ef must be at most {MAX_EF}")));
        }
        if let Some(f) = &req.filter {
            f.validate().map_err(Error::BadRequest)?;
        }
        if let Some(r) = req.rerank {
            if r == 0 || r > MAX_RERANK || req.k * r > MAX_RERANK_CANDIDATES {
                return Err(Error::BadRequest(format!("rerank must be 1..{MAX_RERANK} and k * rerank at most {MAX_RERANK_CANDIDATES}")));
            }
        }
        let inner = self.inner.read();
        let q = inner.store.prepare(&req.vector)?;
        let store = &inner.store;
        let filt = req.filter.as_ref();
        let allow = |i: u32| !store.deleted[i as usize] && filt.is_none_or(|f| store.metadata[i as usize].as_ref().is_some_and(|m| f.matches(m)));
        let (res, plan) = match req.mode {
            Mode::Exact => (exact_search(store, &q, req.k, Some(&allow)), "exact".to_string()),
            Mode::Pq => {
                let pq = inner.pq.as_ref().ok_or_else(|| Error::BadRequest("no PQ index trained (POST /train_pq)".into()))?;
                (pq.search(store, &q, req.k, req.rerank.unwrap_or(10), Some(&allow)), "pq".to_string())
            }
            Mode::Hnsw => match filt {
                None => (inner.hnsw.search(store, &q, req.k, ef, Some(&allow)), "hnsw".to_string()),
                Some(f) => {
                    let sel = estimate_selectivity(store, f);
                    if sel < EXACT_FALLBACK_SELECTIVITY {
                        (exact_search(store, &q, req.k, Some(&allow)), format!("exact-filter-fallback (selectivity≈{sel:.3})"))
                    } else {
                        let ef2 = ((ef as f64 / sel).ceil() as usize).clamp(ef, MAX_EF);
                        (inner.hnsw.search(store, &q, req.k, ef2, Some(&allow)), format!("hnsw-filtered ef={ef2} (selectivity≈{sel:.3})"))
                    }
                }
            },
        };
        let hits = res.into_iter().map(|(d, s)| Hit { id: store.ext_ids[s as usize], distance: d, metadata: store.metadata[s as usize].clone() }).collect();
        self.queries.fetch_add(1, Ordering::Relaxed);
        Ok(SearchResponse { hits, plan, micros: t0.elapsed().as_micros() as u64 })
    }

    /// Writes a snapshot of the current state and empties the WAL.
    pub fn snapshot(&self) -> Result<u64, Error> {
        let mut wal = self.wal.lock(); // no writes while snapshotting
        let inner = self.inner.read();
        let st = State { lsn: inner.lsn, store: inner.store.clone(), hnsw: clone_hnsw(&inner.hnsw), pq: inner.pq.clone() };
        drop(inner);
        snapshot::write_atomic(&self.dir, &st)?;
        // A crash here leaves WAL records with lsn <= snapshot lsn; recovery skips them.
        wal.reset()?;
        Ok(st.lsn)
    }

    /// Compacts tombstones away and rebuilds the HNSW graph (and PQ codes with
    /// the existing codebooks) from the live vectors, then snapshots.
    pub fn rebuild(&self) -> Result<Stats, Error> {
        {
            let _wal = self.wal.lock();
            let mut inner = self.inner.write();
            let old = &inner.store;
            let mut store = VectorStore::new(old.dim, old.metric);
            for i in 0..old.len() {
                if !old.deleted[i] {
                    store.push(old.ext_ids[i], old.vector(i as u32), old.metadata[i].clone())?;
                }
            }
            let mut hnsw = Hnsw::new(self.config.params());
            for i in 0..store.len() as u32 {
                hnsw.insert(&store, i);
            }
            let pq = inner.pq.as_ref().map(|p| {
                let mut np = Pq { codes: Vec::new(), ..p.clone() };
                for i in 0..store.len() as u32 {
                    np.push(store.vector(i));
                }
                np
            });
            inner.store = store;
            inner.hnsw = hnsw;
            inner.pq = pq;
        }
        self.snapshot()?;
        Ok(self.stats())
    }

    pub fn train_pq(&self, m: usize, max_train: usize) -> Result<Stats, Error> {
        {
            let _wal = self.wal.lock();
            let mut inner = self.inner.write();
            let pq = Pq::train(&inner.store, m, max_train, 20, self.config.seed).map_err(Error::BadRequest)?;
            inner.pq = Some(pq);
        }
        self.snapshot()?;
        Ok(self.stats())
    }

    pub fn stats(&self) -> Stats {
        // Lock order everywhere is wal -> inner; read the WAL size first and
        // release it (taking it while holding `inner` deadlocked with `write`).
        let wal_bytes = self.wal.lock().bytes;
        let inner = self.inner.read();
        Stats {
            dim: inner.store.dim,
            metric: inner.store.metric,
            live: inner.store.live,
            slots: inner.store.len(),
            deleted_fraction: inner.store.deleted_fraction(),
            lsn: inner.lsn,
            wal_bytes,
            hnsw_max_level: inner.hnsw.max_level,
            pq: inner.pq.as_ref().map(|p| (p.m, p.ksub)),
            vector_bytes: inner.store.data.len() * 4,
            graph_bytes: inner.hnsw.memory_bytes(),
            pq_bytes: inner.pq.as_ref().map_or(0, |p| p.memory_bytes()),
            queries: self.queries.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
        }
    }

    pub fn check(&self) -> Result<(), String> {
        let inner = self.inner.read();
        inner.hnsw.check_invariants()?;
        if inner.hnsw.len() != inner.store.len() {
            return Err("graph and store sizes differ".into());
        }
        if let Some(pq) = &inner.pq {
            if pq.codes.len() != inner.store.len() * pq.m {
                return Err("pq codes out of sync".into());
            }
        }
        let live = inner.store.deleted.iter().filter(|d| !**d).count();
        if live != inner.store.live || live != inner.store.by_ext.len() {
            return Err("live count mismatch".into());
        }
        Ok(())
    }
}

fn apply(inner: &mut Inner, op: &Op) -> Result<(), Error> {
    match op {
        Op::Upsert { ext, vector, meta } => {
            let (slot, _) = inner.store.push(*ext, vector, meta.clone())?;
            inner.hnsw.insert(&inner.store, slot);
            if let Some(pq) = &mut inner.pq {
                pq.push(vector);
            }
        }
        Op::Delete { ext } => {
            inner.store.delete(*ext);
        }
    }
    Ok(())
}

fn clone_hnsw(h: &Hnsw) -> Hnsw {
    Hnsw::from_parts(h.params, h.levels.clone(), h.links.clone(), h.entry, h.max_level, h.params.seed)
}

/// Fraction of live slots matching the filter, estimated from up to 512
/// uniformly sampled slots (exact for small collections).
fn estimate_selectivity(store: &VectorStore, f: &Filter) -> f64 {
    let n = store.len();
    if n == 0 {
        return 1.0;
    }
    let check = |i: usize| !store.deleted[i] && store.metadata[i].as_ref().is_some_and(|m| f.matches(m));
    let live = |i: usize| !store.deleted[i];
    if n <= 512 {
        let l = (0..n).filter(|&i| live(i)).count().max(1);
        return (0..n).filter(|&i| check(i)).count() as f64 / l as f64;
    }
    let mut rng = rand::rngs::SmallRng::seed_from_u64(n as u64);
    let (mut hit, mut seen) = (0usize, 0usize);
    for _ in 0..512 {
        let i = rng.gen_range(0..n);
        if live(i) {
            seen += 1;
            hit += check(i) as usize;
        }
    }
    if seen == 0 {
        0.0
    } else {
        hit as f64 / seen as f64
    }
}
