//! REST and gRPC front-ends against a real server on an ephemeral port.
use serde_json::{json, Value};
use std::future::IntoFuture;
use std::sync::Arc;
use vectordb::server::pb::vector_db_client::VectorDbClient;
use vectordb::server::{grpc_service, pb, router, Registry};
use vectordb::wal::SyncMode;

struct Running {
    http: String,
    grpc: String,
    stop: tokio::sync::watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Running {
    /// Graceful shutdown of both servers; afterwards the registry (and the
    /// collection directory locks it holds) has been dropped.
    async fn stop(self) {
        self.stop.send(true).unwrap();
        for t in self.tasks {
            t.await.unwrap();
        }
    }
}

async fn start(dir: &std::path::Path) -> Running {
    let reg = Registry::open(dir.to_path_buf(), SyncMode::Never).unwrap();
    let (tx, rx) = tokio::sync::watch::channel(false);
    let wait = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|v| *v).await;
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = format!("http://{}", l.local_addr().unwrap());
    let h = axum::serve(l, router(Arc::clone(&reg))).with_graceful_shutdown(wait(rx.clone())).into_future();
    let gl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc = format!("http://{}", gl.local_addr().unwrap());
    let g = tonic::transport::Server::builder().add_service(grpc_service(reg)).serve_with_incoming_shutdown(async_stream_listener(gl), wait(rx));
    let tasks = vec![tokio::spawn(async move { h.await.unwrap() }), tokio::spawn(async move { g.await.unwrap() })];
    Running { http, grpc, stop: tx, tasks }
}

fn async_stream_listener(l: tokio::net::TcpListener) -> impl futures_core::Stream<Item = std::io::Result<tokio::net::TcpStream>> {
    struct S(tokio::net::TcpListener);
    impl futures_core::Stream for S {
        type Item = std::io::Result<tokio::net::TcpStream>;
        fn poll_next(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
            self.0.poll_accept(cx).map(|r| Some(r.map(|(s, _)| s)))
        }
    }
    S(l)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_lifecycle_and_errors() {
    let d = tempfile::tempdir().unwrap();
    let srv = start(d.path()).await;
    let http = srv.http.clone();
    let c = reqwest::Client::new();
    let cfg = json!({"dim": 3, "metric": "l2", "m": 8, "ef_construction": 32, "seed": 1});
    assert_eq!(c.put(format!("{http}/collections/docs")).json(&cfg).send().await.unwrap().status(), 200);
    // idempotent create, conflicting create
    assert_eq!(c.put(format!("{http}/collections/docs")).json(&cfg).send().await.unwrap().status(), 200);
    let mut other = cfg.clone();
    other["dim"] = json!(4);
    assert_eq!(c.put(format!("{http}/collections/docs")).json(&other).send().await.unwrap().status(), 409);
    assert_eq!(c.put(format!("{http}/collections/..%2Fetc")).json(&cfg).send().await.unwrap().status(), 422);
    let pts = json!([
        {"id": 1, "vector": [0.0, 0.0, 0.0], "metadata": {"lang": "en"}},
        {"id": 2, "vector": [1.0, 0.0, 0.0], "metadata": {"lang": "de"}},
        {"id": 3, "vector": [5.0, 5.0, 5.0], "metadata": {"lang": "en"}}
    ]);
    assert_eq!(c.put(format!("{http}/collections/docs/points")).json(&pts).send().await.unwrap().status(), 200);
    let r: Value = c.post(format!("{http}/collections/docs/search")).json(&json!({"vector": [0.9, 0.0, 0.0], "k": 2})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["hits"][0]["id"], 2);
    let r: Value = c
        .post(format!("{http}/collections/docs/search"))
        .json(&json!({"vector": [0.9, 0.0, 0.0], "k": 2, "filter": {"eq": ["lang", "en"]}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["hits"].as_array().unwrap().iter().map(|h| h["id"].as_u64().unwrap()).collect::<Vec<_>>(), vec![1, 3]);
    let bad = c.post(format!("{http}/collections/docs/search")).json(&json!({"vector": [0.9, 0.0], "k": 2})).send().await.unwrap();
    assert_eq!(bad.status(), 422);
    assert_eq!(c.post(format!("{http}/collections/nope/search")).json(&json!({"vector": [0.0, 0.0, 0.0], "k": 1})).send().await.unwrap().status(), 404);
    assert_eq!(c.delete(format!("{http}/collections/docs/points/2")).send().await.unwrap().status(), 204);
    assert_eq!(c.get(format!("{http}/collections/docs/points/2")).send().await.unwrap().status(), 404);
    let m = c.get(format!("{http}/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(m.contains("vdb_live_vectors{collection=\"docs\"} 2") && m.contains("vdb_search_latency_seconds_count 2"));
    assert_eq!(c.post(format!("{http}/collections/docs/snapshot")).send().await.unwrap().status(), 200);
    assert_eq!(c.delete(format!("{http}/collections/docs")).send().await.unwrap().status(), 204);
    assert!(!d.path().join("docs").exists());
    drop(c);
    srv.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_round_trip_and_restart_recovery() {
    let d = tempfile::tempdir().unwrap();
    {
        let srv = start(d.path()).await;
        let (http, grpc) = (srv.http.clone(), srv.grpc.clone());
        reqwest::Client::new().put(format!("{http}/collections/g")).json(&json!({"dim": 2, "metric": "cosine", "m": 8, "ef_construction": 32, "seed": 1})).send().await.unwrap();
        let mut cl = VectorDbClient::connect(grpc).await.unwrap();
        let pts = (0..50u64).map(|i| pb::Point { id: i, vector: vec![(i as f32).cos(), (i as f32).sin()], metadata_json: format!("{{\"i\": {i}}}") }).collect();
        let up = cl.upsert(pb::UpsertRequest { collection: "g".into(), points: pts }).await.unwrap().into_inner();
        assert_eq!(up.upserted, 50);
        let r = cl.search(pb::SearchRequest { collection: "g".into(), vector: vec![1.0, 0.0], k: 1, ..Default::default() }).await.unwrap().into_inner();
        assert_eq!(r.hits[0].id, 0);
        let err = cl.search(pb::SearchRequest { collection: "g".into(), vector: vec![1.0, 0.0], k: 1, filter_json: "{bad".into(), ..Default::default() }).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(cl.delete(pb::DeleteRequest { collection: "g".into(), id: 0 }).await.unwrap().into_inner().deleted);
        // while this server runs, a second registry cannot open the same data
        assert!(Registry::open(d.path().to_path_buf(), SyncMode::Never).is_err());
        drop(cl);
        srv.stop().await;
    }
    // restart: a new registry over the same directory recovers from the WAL
    let srv = start(d.path()).await;
    let mut cl = VectorDbClient::connect(srv.grpc.clone()).await.unwrap();
    let st: Value = serde_json::from_str(&cl.stats(pb::StatsRequest { collection: "g".into() }).await.unwrap().into_inner().stats_json).unwrap();
    assert_eq!(st["live"], 49);
    let r = cl.search(pb::SearchRequest { collection: "g".into(), vector: vec![1.0, 0.0], k: 1, ..Default::default() }).await.unwrap().into_inner();
    assert_ne!(r.hits[0].id, 0);
    drop(cl);
    srv.stop().await;
}
