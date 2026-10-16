//! REST (axum) and gRPC (tonic) front-ends over a registry of collections.
//!
//! Collections live in `<data_dir>/<name>/`; names are restricted to
//! `[a-z0-9_-]{1,64}` so they can never escape the data directory. All
//! index work runs on tokio's blocking pool. A Prometheus-style `/metrics`
//! endpoint reports per-collection counters and a global search latency
//! histogram.
#![allow(clippy::result_large_err)] // tonic::Status is the gRPC error type

use crate::collection::{Collection, Config, Error as CollError, Mode, SearchRequest};
use crate::filter::{Filter, Metadata};
use crate::wal::SyncMode;
use axum::extract::{DefaultBodyLimit, Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use parking_lot::RwLock;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub mod pb {
    tonic::include_proto!("vectordb.v1");
}

const BUCKETS_US: [u64; 10] = [100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 100_000, u64::MAX];

pub struct Registry {
    pub dir: PathBuf,
    pub sync: SyncMode,
    cols: RwLock<HashMap<String, Arc<Collection>>>,
    hist: [AtomicU64; 10],
    hist_sum_us: AtomicU64,
}

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    Bad(String),
    Conflict(String),
    Internal(String),
}

impl From<CollError> for ApiError {
    fn from(e: CollError) -> Self {
        match e {
            CollError::Invalid(x) => ApiError::Bad(x.to_string()),
            CollError::BadRequest(x) => ApiError::Bad(x),
            CollError::Config(x) => ApiError::Conflict(x),
            CollError::Locked(p) => ApiError::Conflict(format!("{} is locked by another handle", p.display())),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Bad(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::Internal(m) => {
                tracing::error!("internal error: {m}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl Registry {
    /// Opens every collection found in `dir` (recovering each).
    pub fn open(dir: PathBuf, sync: SyncMode) -> Result<Arc<Registry>, String> {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let reg = Registry { dir: dir.clone(), sync, cols: RwLock::new(HashMap::new()), hist: Default::default(), hist_sum_us: AtomicU64::new(0) };
        for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(".dropped-") {
                // a drop that crashed before deleting its renamed directory
                let _ = std::fs::remove_dir_all(e.path());
                continue;
            }
            if !valid_name(&name) || !e.path().join("config.json").exists() {
                continue;
            }
            let cfg: Config = serde_json::from_slice(&std::fs::read(e.path().join("config.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let c = Collection::open(&e.path(), cfg, sync).map_err(|err| format!("collection {name}: {err}"))?;
            tracing::info!(collection = %name, recovered = ?c.recovered, "opened");
            reg.cols.write().insert(name, Arc::new(c));
        }
        Ok(Arc::new(reg))
    }

    pub fn get(&self, name: &str) -> Result<Arc<Collection>, ApiError> {
        self.cols.read().get(name).cloned().ok_or_else(|| ApiError::NotFound(format!("collection '{name}' not found")))
    }

    pub fn create(&self, name: &str, cfg: Config) -> Result<Arc<Collection>, ApiError> {
        if !valid_name(name) {
            return Err(ApiError::Bad("collection names are 1-64 chars of [a-z0-9_-]".into()));
        }
        let mut cols = self.cols.write();
        if let Some(c) = cols.get(name) {
            return if c.config == cfg { Ok(c.clone()) } else { Err(ApiError::Conflict(format!("collection '{name}' exists with {:?}", c.config))) };
        }
        let c = Arc::new(Collection::open(&self.dir.join(name), cfg, self.sync)?);
        cols.insert(name.to_string(), c.clone());
        Ok(c)
    }

    /// Unregisters the collection and deletes its directory. The directory is
    /// first renamed to a hidden name while the registry lock is held, so a
    /// concurrent `create` of the same name always starts a fresh directory
    /// (it can never re-open the one being deleted). In-flight requests that
    /// still hold the old handle finish against the renamed files.
    pub fn drop_collection(&self, name: &str) -> Result<(), ApiError> {
        let doomed = {
            let mut cols = self.cols.write();
            let c = cols.get(name).cloned().ok_or_else(|| ApiError::NotFound(format!("collection '{name}' not found")))?;
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            let doomed = self.dir.join(format!(".dropped-{name}-{nanos}"));
            std::fs::rename(&c.dir, &doomed).map_err(|e| ApiError::Internal(e.to_string()))?;
            cols.remove(name);
            doomed
        };
        std::fs::remove_dir_all(doomed).map_err(|e| ApiError::Internal(e.to_string()))
    }

    pub fn observe(&self, micros: u64) {
        let i = BUCKETS_US.iter().position(|&b| micros <= b).unwrap_or(BUCKETS_US.len() - 1);
        self.hist[i].fetch_add(1, Ordering::Relaxed);
        self.hist_sum_us.fetch_add(micros, Ordering::Relaxed);
    }

    pub fn metrics(&self) -> String {
        let mut s = String::from("# TYPE vdb_search_latency_seconds histogram\n");
        let mut cum = 0;
        for (i, b) in BUCKETS_US.iter().enumerate() {
            cum += self.hist[i].load(Ordering::Relaxed);
            let le = if *b == u64::MAX { "+Inf".to_string() } else { format!("{}", *b as f64 / 1e6) };
            s += &format!("vdb_search_latency_seconds_bucket{{le=\"{le}\"}} {cum}\n");
        }
        s += &format!("vdb_search_latency_seconds_sum {}\nvdb_search_latency_seconds_count {cum}\n", self.hist_sum_us.load(Ordering::Relaxed) as f64 / 1e6);
        for (name, c) in self.cols.read().iter() {
            let st = c.stats();
            s += &format!("vdb_live_vectors{{collection=\"{name}\"}} {}\nvdb_slots{{collection=\"{name}\"}} {}\n", st.live, st.slots);
            s += &format!("vdb_queries_total{{collection=\"{name}\"}} {}\nvdb_writes_total{{collection=\"{name}\"}} {}\n", st.queries, st.writes);
            s += &format!("vdb_wal_bytes{{collection=\"{name}\"}} {}\nvdb_deleted_fraction{{collection=\"{name}\"}} {}\n", st.wal_bytes, st.deleted_fraction);
        }
        s
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::Internal(e.to_string()))?
}

#[derive(Deserialize)]
struct PointIn {
    id: u64,
    vector: Vec<f32>,
    #[serde(default)]
    metadata: Option<Metadata>,
}

#[derive(Deserialize)]
struct PqIn {
    m: usize,
    #[serde(default = "default_train")]
    max_train: usize,
}
fn default_train() -> usize {
    100_000
}

type St = State<Arc<Registry>>;

pub fn router(reg: Arc<Registry>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/metrics", get(|State(r): St| async move { r.metrics() }))
        .route("/collections", get(|State(r): St| async move { Json(r.cols.read().keys().cloned().collect::<Vec<_>>()) }))
        .route(
            "/collections/:name",
            put(|State(r): St, AxPath(name): AxPath<String>, Json(cfg): Json<Config>| async move { blocking(move || r.create(&name, cfg).map(|c| Json(c.stats()))).await })
                .get(|State(r): St, AxPath(name): AxPath<String>| async move { r.get(&name).map(|c| Json(c.stats())) })
                .delete(|State(r): St, AxPath(name): AxPath<String>| async move { blocking(move || r.drop_collection(&name).map(|_| StatusCode::NO_CONTENT)).await }),
        )
        .route(
            "/collections/:name/points",
            put(|State(r): St, AxPath(name): AxPath<String>, Json(points): Json<Vec<PointIn>>| async move {
                let c = r.get(&name)?;
                if points.len() > 10_000 {
                    return Err(ApiError::Bad("at most 10000 points per request".into()));
                }
                let n = points.len();
                blocking(move || {
                    let lsn = c.upsert_batch(points.into_iter().map(|p| (p.id, p.vector, p.metadata)).collect())?;
                    Ok(Json(serde_json::json!({ "upserted": n, "lsn": lsn })))
                })
                .await
            }),
        )
        .route(
            "/collections/:name/points/:id",
            get(|State(r): St, AxPath((name, id)): AxPath<(String, u64)>| async move {
                let c = r.get(&name)?;
                let (v, m) = c.get(id).ok_or_else(|| ApiError::NotFound(format!("point {id} not found")))?;
                Ok::<_, ApiError>(Json(serde_json::json!({ "id": id, "vector": v, "metadata": m })))
            })
            .delete(|State(r): St, AxPath((name, id)): AxPath<(String, u64)>| async move {
                let c = r.get(&name)?;
                blocking(move || if c.delete(id)? { Ok(StatusCode::NO_CONTENT) } else { Err(ApiError::NotFound(format!("point {id} not found"))) }).await
            }),
        )
        .route(
            "/collections/:name/search",
            post(|State(r): St, AxPath(name): AxPath<String>, Json(req): Json<SearchRequest>| async move {
                let c = r.get(&name)?;
                let r2 = r.clone();
                blocking(move || {
                    let res = c.search(&req)?;
                    r2.observe(res.micros);
                    Ok(Json(res))
                })
                .await
            }),
        )
        .route(
            "/collections/:name/snapshot",
            post(|State(r): St, AxPath(name): AxPath<String>| async move {
                let c = r.get(&name)?;
                blocking(move || Ok(Json(serde_json::json!({ "lsn": c.snapshot()? })))).await
            }),
        )
        .route(
            "/collections/:name/rebuild",
            post(|State(r): St, AxPath(name): AxPath<String>| async move {
                let c = r.get(&name)?;
                blocking(move || Ok(Json(c.rebuild()?))).await
            }),
        )
        .route(
            "/collections/:name/train_pq",
            post(|State(r): St, AxPath(name): AxPath<String>, Json(p): Json<PqIn>| async move {
                let c = r.get(&name)?;
                blocking(move || Ok(Json(c.train_pq(p.m, p.max_train)?))).await
            }),
        )
        .layer(DefaultBodyLimit::max(64 << 20))
        .with_state(reg)
}

// ---------------------------------------------------------------- gRPC

pub struct Grpc(pub Arc<Registry>);

/// Largest gRPC request or response message (same as the REST body limit).
pub const GRPC_MAX_MESSAGE: usize = 64 << 20;

/// The gRPC service with message-size limits applied in both directions.
pub fn grpc_service(reg: Arc<Registry>) -> pb::vector_db_server::VectorDbServer<Grpc> {
    pb::vector_db_server::VectorDbServer::new(Grpc(reg)).max_decoding_message_size(GRPC_MAX_MESSAGE).max_encoding_message_size(GRPC_MAX_MESSAGE)
}

fn status(e: ApiError) -> tonic::Status {
    match e {
        ApiError::NotFound(m) => tonic::Status::not_found(m),
        ApiError::Bad(m) => tonic::Status::invalid_argument(m),
        ApiError::Conflict(m) => tonic::Status::already_exists(m),
        ApiError::Internal(m) => tonic::Status::internal(m),
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(s: &str, what: &str) -> Result<Option<T>, tonic::Status> {
    if s.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(s).map(Some).map_err(|e| tonic::Status::invalid_argument(format!("{what}: {e}")))
}

#[tonic::async_trait]
impl pb::vector_db_server::VectorDb for Grpc {
    async fn upsert(&self, req: tonic::Request<pb::UpsertRequest>) -> Result<tonic::Response<pb::UpsertResponse>, tonic::Status> {
        let r = req.into_inner();
        let c = self.0.get(&r.collection).map_err(status)?;
        let pts: Vec<(u64, Vec<f32>, Option<Metadata>)> =
            r.points.into_iter().map(|p| Ok((p.id, p.vector, parse_json(&p.metadata_json, "metadata")?))).collect::<Result<_, tonic::Status>>()?;
        let n = pts.len() as u32;
        if pts.len() > 10_000 {
            return Err(tonic::Status::invalid_argument("at most 10000 points per request"));
        }
        let lsn = blocking(move || Ok(c.upsert_batch(pts)?)).await.map_err(status)?;
        Ok(tonic::Response::new(pb::UpsertResponse { lsn, upserted: n }))
    }

    async fn delete(&self, req: tonic::Request<pb::DeleteRequest>) -> Result<tonic::Response<pb::DeleteResponse>, tonic::Status> {
        let r = req.into_inner();
        let c = self.0.get(&r.collection).map_err(status)?;
        let deleted = blocking(move || Ok(c.delete(r.id)?)).await.map_err(status)?;
        Ok(tonic::Response::new(pb::DeleteResponse { deleted }))
    }

    async fn search(&self, req: tonic::Request<pb::SearchRequest>) -> Result<tonic::Response<pb::SearchResponse>, tonic::Status> {
        let r = req.into_inner();
        let c = self.0.get(&r.collection).map_err(status)?;
        let filter: Option<Filter> = parse_json(&r.filter_json, "filter")?;
        let mode = match r.mode.as_str() {
            "" | "hnsw" => Mode::Hnsw,
            "exact" => Mode::Exact,
            "pq" => Mode::Pq,
            m => return Err(tonic::Status::invalid_argument(format!("unknown mode {m}"))),
        };
        let sr = SearchRequest { vector: r.vector, k: r.k as usize, ef: (r.ef > 0).then_some(r.ef as usize), filter, mode, rerank: (r.rerank > 0).then_some(r.rerank as usize) };
        let reg = self.0.clone();
        let res = blocking(move || {
            let res = c.search(&sr)?;
            reg.observe(res.micros);
            Ok(res)
        })
        .await
        .map_err(status)?;
        Ok(tonic::Response::new(pb::SearchResponse {
            hits: res
                .hits
                .into_iter()
                .map(|h| pb::Hit { id: h.id, distance: h.distance, metadata_json: h.metadata.map(|m| serde_json::to_string(&m).unwrap()).unwrap_or_default() })
                .collect(),
            plan: res.plan,
            micros: res.micros,
        }))
    }

    async fn stats(&self, req: tonic::Request<pb::StatsRequest>) -> Result<tonic::Response<pb::StatsResponse>, tonic::Status> {
        let c = self.0.get(&req.into_inner().collection).map_err(status)?;
        Ok(tonic::Response::new(pb::StatsResponse { stats_json: serde_json::to_string(&c.stats()).unwrap() }))
    }
}
