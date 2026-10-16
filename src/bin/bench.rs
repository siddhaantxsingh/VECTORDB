//! Benchmark harness.
//!
//! `vdb-bench --out DIR [--dataset lowrank|mixture] [--n 100000] [--dim 128]
//!            [--queries 1000] [--m 16] [--ef-construction 200] [--seed 7] [--skip-pq]`
//!
//! `--dataset lowrank` (default) or `--dataset mixture`; see the generators.
//!
//! Generates a deterministic synthetic dataset, writes it as
//! `base.fvecs` / `query.fvecs` / `groundtruth.ivecs` (top-100 exact L2
//! neighbours, so other engines can be measured on identical data), then
//! measures through the public `Collection` API:
//!
//! * build (insert) time and throughput, with the WAL in `SyncMode::Never`
//! * recall@10 against exact search and QPS / p50 / p95 / p99 latency, per ef,
//!   on 1 and 2 threads
//! * memory: raw vector bytes, HNSW graph bytes, process RSS
//! * filtered queries at 50 %, 10 % and 1 % selectivity (recall against exact
//!   filtered search, with the plan the engine chose)
//! * PQ: training time, recall@10 at several rerank factors, code bytes
//! * snapshot size and write time; recovery time from snapshot and from WAL
//!
//! Everything is written to `DIR/results.json`.
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use vectordb::collection::{Collection, Config, Mode, SearchRequest};
use vectordb::distance::Metric;
use vectordb::filter::{Filter, Metadata, Range, Value};
use vectordb::wal::SyncMode;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn gauss(r: &mut SmallRng) -> f32 {
    // Box-Muller
    let u1: f64 = r.gen_range(f64::MIN_POSITIVE..1.0);
    let u2: f64 = r.gen();
    ((-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()) as f32
}

/// `n` points from a mixture of 64 Gaussians in the full space (centres ~
/// N(0, 1), spread 0.35). High intrinsic dimensionality: neighbours inside a
/// cluster are nearly equidistant, a hard case for any graph index.
fn mixture(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut r = SmallRng::seed_from_u64(seed);
    let centres: Vec<Vec<f32>> = (0..64).map(|_| (0..dim).map(|_| gauss(&mut r)).collect()).collect();
    (0..n)
        .map(|_| {
            let c = &centres[r.gen_range(0..centres.len())];
            c.iter().map(|x| x + 0.35 * gauss(&mut r)).collect()
        })
        .collect()
}

/// `n` points with low intrinsic dimension, closer to real embeddings: a
/// 64-component Gaussian mixture in a 16-d latent space, mapped to `dim`
/// dimensions by a fixed random linear map, plus isotropic noise (0.05).
fn lowrank(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    const LATENT: usize = 16;
    let mut r = SmallRng::seed_from_u64(seed);
    let a: Vec<f32> = (0..dim * LATENT).map(|_| gauss(&mut r) / (LATENT as f32).sqrt()).collect();
    let centres: Vec<Vec<f32>> = (0..64).map(|_| (0..LATENT).map(|_| gauss(&mut r)).collect()).collect();
    (0..n)
        .map(|_| {
            let c = &centres[r.gen_range(0..centres.len())];
            let z: Vec<f32> = c.iter().map(|x| x + 0.35 * gauss(&mut r)).collect();
            (0..dim).map(|i| (0..LATENT).map(|j| a[i * LATENT + j] * z[j]).sum::<f32>() + 0.05 * gauss(&mut r)).collect()
        })
        .collect()
}

type Generator = fn(usize, usize, u64) -> Vec<Vec<f32>>;

fn write_fvecs(p: &Path, vs: &[Vec<f32>]) -> std::io::Result<()> {
    let mut w = std::io::BufWriter::new(std::fs::File::create(p)?);
    for v in vs {
        w.write_all(&(v.len() as i32).to_le_bytes())?;
        for x in v {
            w.write_all(&x.to_le_bytes())?;
        }
    }
    w.flush()
}

fn write_ivecs(p: &Path, vs: &[Vec<u64>]) -> std::io::Result<()> {
    let mut w = std::io::BufWriter::new(std::fs::File::create(p)?);
    for v in vs {
        w.write_all(&(v.len() as i32).to_le_bytes())?;
        for x in v {
            w.write_all(&(*x as i32).to_le_bytes())?;
        }
    }
    w.flush()
}

fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .map_or(0, |kb| kb * 1024)
}

fn dir_bytes(p: &Path) -> u64 {
    std::fs::read_dir(p).map(|d| d.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()).unwrap_or(0)
}

fn recall(found: &[u64], truth: &[u64], k: usize) -> f64 {
    let t = &truth[..k.min(truth.len())];
    if t.is_empty() {
        return 1.0;
    }
    found.iter().take(k).filter(|id| t.contains(id)).count() as f64 / t.len() as f64
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

fn req(v: &[f32], k: usize, ef: Option<usize>, filter: Option<Filter>, mode: Mode, rerank: Option<usize>) -> SearchRequest {
    SearchRequest { vector: v.to_vec(), k, ef, filter, mode, rerank }
}

/// Runs every query once on `threads` threads; returns (ids per query, latencies in µs, wall seconds).
type Batch = (Vec<Vec<u64>>, Vec<f64>, f64);

fn run(c: &Arc<Collection>, reqs: &[SearchRequest], threads: usize) -> Batch {
    let t0 = Instant::now();
    type PerThread = (usize, Vec<(Vec<u64>, f64)>);
    let chunks: Vec<PerThread> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let c = Arc::clone(c);
                s.spawn(move || {
                    let mut out = Vec::new();
                    for (i, r) in reqs.iter().enumerate() {
                        if i % threads != t {
                            continue;
                        }
                        let q0 = Instant::now();
                        let res = c.search(r).expect("search");
                        out.push((res.hits.iter().map(|h| h.id).collect(), q0.elapsed().as_secs_f64() * 1e6));
                    }
                    (t, out)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let wall = t0.elapsed().as_secs_f64();
    let mut ids = vec![Vec::new(); reqs.len()];
    let mut lat = Vec::with_capacity(reqs.len());
    for (t, out) in chunks {
        for (j, (v, l)) in out.into_iter().enumerate() {
            ids[t + j * threads] = v;
            lat.push(l);
        }
    }
    lat.sort_by(|a, b| a.total_cmp(b));
    (ids, lat, wall)
}

fn summary(ids: &[Vec<u64>], truth: &[Vec<u64>], lat: &[f64], wall: f64, k: usize) -> J {
    let r = ids.iter().zip(truth).map(|(a, b)| recall(a, b, k)).sum::<f64>() / ids.len().max(1) as f64;
    json!({
        "recall_at_10": (r * 10000.0).round() / 10000.0,
        "qps": (ids.len() as f64 / wall).round(),
        "p50_us": pct(lat, 0.50).round(), "p95_us": pct(lat, 0.95).round(), "p99_us": pct(lat, 0.99).round(),
    })
}

fn meta(i: usize) -> Metadata {
    let mut m = BTreeMap::new();
    m.insert("bucket".to_string(), Value::Int((i % 100) as i64));
    m
}

fn below(b: i64) -> Filter {
    Filter::Range(Range { field: "bucket".into(), gte: None, lte: Some((b - 1) as f64) })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out: PathBuf = arg(&args, "--out", PathBuf::from("bench-out"));
    let n: usize = arg(&args, "--n", 100_000);
    let dim: usize = arg(&args, "--dim", 128);
    let nq: usize = arg(&args, "--queries", 1000);
    let m: usize = arg(&args, "--m", 16);
    let efc: usize = arg(&args, "--ef-construction", 200);
    let seed: u64 = arg(&args, "--seed", 7);
    let skip_pq = args.iter().any(|a| a == "--skip-pq");
    let kind: String = arg(&args, "--dataset", "lowrank".to_string());
    let (gen, desc): (Generator, &str) = match kind.as_str() {
        "mixture" => (mixture, "mixture of 64 Gaussians in the full space, centres N(0,1), spread 0.35"),
        "lowrank" => (lowrank, "64-component Gaussian mixture in a 16-d latent space, random linear map to dim, noise 0.05"),
        other => panic!("unknown --dataset {other} (mixture | lowrank)"),
    };
    let k = 10;
    std::fs::create_dir_all(&out).unwrap();
    let mut res = serde_json::Map::new();
    res.insert(
        "setup".into(),
        json!({"n": n, "dim": dim, "queries": nq, "m": m, "ef_construction": efc, "seed": seed, "metric": "l2",
               "kernel": vectordb::distance::kernel_name(),
               "cpus": std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1),
               "wal_sync": "never", "dataset": kind, "dataset_description": desc}),
    );
    eprintln!("generating {n} x {dim} (+{nq} queries)");
    // queries come from the same distribution (same centres) as the base set
    let mut base = gen(n + nq, dim, seed);
    let queries = base.split_off(n);
    write_fvecs(&out.join("base.fvecs"), &base).unwrap();
    write_fvecs(&out.join("query.fvecs"), &queries).unwrap();
    let rss0 = rss_bytes();

    // -------- build
    let data = out.join("collection");
    let _ = std::fs::remove_dir_all(&data);
    let cfg = Config { dim, metric: Metric::L2, m, ef_construction: efc, seed };
    let c = Arc::new(Collection::open(&data, cfg.clone(), SyncMode::Never).unwrap());
    let t0 = Instant::now();
    for (i, v) in base.iter().enumerate() {
        c.upsert(i as u64, v, Some(meta(i))).unwrap();
        if i > 0 && i % 20_000 == 0 {
            eprintln!("  inserted {i} ({:.1}s)", t0.elapsed().as_secs_f64());
        }
    }
    let build_s = t0.elapsed().as_secs_f64();
    c.check().expect("invariants after build");
    let st = c.stats();
    res.insert("build".into(), json!({"seconds": (build_s * 100.0).round() / 100.0, "inserts_per_sec": (n as f64 / build_s).round(), "hnsw_max_level": st.hnsw_max_level}));
    res.insert(
        "memory".into(),
        json!({"vector_bytes": st.vector_bytes, "graph_bytes": st.graph_bytes, "graph_bytes_per_vector": st.graph_bytes / n.max(1),
               "rss_bytes": rss_bytes(), "rss_before_build_bytes": rss0, "wal_bytes": st.wal_bytes}),
    );

    // -------- ground truth (exact search through the engine)
    eprintln!("ground truth");
    let exact: Vec<SearchRequest> = queries.iter().map(|q| req(q, 100, None, None, Mode::Exact, None)).collect();
    let (truth, lat, wall) = run(&c, &exact, 2);
    write_ivecs(&out.join("groundtruth.ivecs"), &truth).unwrap();
    res.insert("exact".into(), summary(&truth, &truth, &lat, wall, k));

    // -------- recall / QPS vs ef
    eprintln!("hnsw sweep");
    let mut sweep = Vec::new();
    for ef in [10usize, 16, 32, 64, 128, 256] {
        let rs: Vec<_> = queries.iter().map(|q| req(q, k, Some(ef), None, Mode::Hnsw, None)).collect();
        let _ = run(&c, &rs[..rs.len().min(100)], 1); // warm-up
        let (i1, l1, w1) = run(&c, &rs, 1);
        let (i2, l2, w2) = run(&c, &rs, 2);
        sweep.push(json!({"ef": ef, "threads_1": summary(&i1, &truth, &l1, w1, k), "threads_2": summary(&i2, &truth, &l2, w2, k)}));
        eprintln!("  ef={ef} {}", sweep.last().unwrap()["threads_1"]);
    }
    res.insert("hnsw".into(), J::Array(sweep));

    // -------- filtered
    eprintln!("filtered");
    let mut filtered = Vec::new();
    for (label, b) in [("50%", 50), ("10%", 10), ("1%", 1)] {
        let f = below(b);
        let ex: Vec<_> = queries.iter().map(|q| req(q, k, None, Some(f.clone()), Mode::Exact, None)).collect();
        let (ftruth, _, _) = run(&c, &ex, 2);
        let rs: Vec<_> = queries.iter().map(|q| req(q, k, Some(64), Some(f.clone()), Mode::Hnsw, None)).collect();
        let plan = c.search(&rs[0]).unwrap().plan;
        let (i1, l1, w1) = run(&c, &rs, 1);
        let mut s = summary(&i1, &ftruth, &l1, w1, k);
        s["selectivity"] = json!(label);
        s["plan"] = json!(plan);
        eprintln!("  {label} {s}");
        filtered.push(s);
    }
    res.insert("filtered_ef64_threads_1".into(), J::Array(filtered));

    // -------- PQ
    if !skip_pq {
        eprintln!("pq");
        let pm = (1..=dim).rev().find(|d| dim.is_multiple_of(*d) && dim / d >= 4 && *d <= 32).unwrap_or(1);
        let t0 = Instant::now();
        let st = c.train_pq(pm, 50_000).unwrap(); // also writes a snapshot
        let train_s = t0.elapsed().as_secs_f64();
        let mut pq = Vec::new();
        for rerank in [1usize, 4, 16] {
            let rs: Vec<_> = queries.iter().map(|q| req(q, k, None, None, Mode::Pq, Some(rerank))).collect();
            let (i1, l1, w1) = run(&c, &rs, 1);
            let mut s = summary(&i1, &truth, &l1, w1, k);
            s["rerank"] = json!(rerank);
            pq.push(s);
        }
        res.insert(
            "pq".into(),
            json!({"m": pm, "ksub": st.pq.map(|p| p.1), "train_seconds": (train_s * 100.0).round() / 100.0, "train_sample": 50_000.min(n),
                   "code_and_codebook_bytes": st.pq_bytes, "compression_vs_f32": (st.vector_bytes as f64 / st.pq_bytes.max(1) as f64 * 10.0).round() / 10.0,
                   "search_threads_1": pq}),
        );
    }

    // -------- snapshot + recovery
    eprintln!("snapshot / recovery");
    let t0 = Instant::now();
    c.snapshot().unwrap();
    let snap_s = t0.elapsed().as_secs_f64();
    let snap_bytes = std::fs::metadata(data.join("snapshot.vdb")).map(|m| m.len()).unwrap_or(0);
    let probe = req(&queries[0], k, Some(64), None, Mode::Hnsw, None);
    let before = c.search(&probe).unwrap().hits;
    drop(c);
    let t0 = Instant::now();
    let c2 = Collection::open(&data, cfg.clone(), SyncMode::Never).unwrap();
    let from_snap_s = t0.elapsed().as_secs_f64();
    assert_eq!(c2.search(&probe).unwrap().hits, before, "results differ after recovery from snapshot");
    drop(c2);
    // WAL-only recovery on a 10 % prefix (replay re-inserts into the graph)
    let wal_dir = out.join("collection-wal");
    let _ = std::fs::remove_dir_all(&wal_dir);
    let wn = (n / 10).max(1);
    {
        let cw = Collection::open(&wal_dir, cfg.clone(), SyncMode::Never).unwrap();
        for (i, v) in base.iter().take(wn).enumerate() {
            cw.upsert(i as u64, v, Some(meta(i))).unwrap();
        }
    }
    let t0 = Instant::now();
    let cw = Collection::open(&wal_dir, cfg, SyncMode::Never).unwrap();
    let from_wal_s = t0.elapsed().as_secs_f64();
    assert_eq!(cw.stats().live, wn);
    res.insert(
        "durability".into(),
        json!({"snapshot_seconds": (snap_s * 1000.0).round() / 1000.0, "snapshot_bytes": snap_bytes, "data_dir_bytes": dir_bytes(&data),
               "recover_from_snapshot_seconds": (from_snap_s * 1000.0).round() / 1000.0,
               "recover_from_wal_records": wn, "recover_from_wal_seconds": (from_wal_s * 1000.0).round() / 1000.0,
               "results_identical_after_recovery": true}),
    );
    drop(cw);
    let _ = std::fs::remove_dir_all(&wal_dir);
    let _ = std::fs::remove_dir_all(&data);

    let s = serde_json::to_string_pretty(&J::Object(res)).unwrap();
    std::fs::write(out.join("results.json"), &s).unwrap();
    println!("{s}");
}
