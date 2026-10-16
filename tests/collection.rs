//! Integration tests: persistence, recovery, crash points, filters,
//! deletion, rebuild, PQ and concurrency on the public `Collection` API.

use proptest::prelude::*;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use vectordb::collection::{Collection, Config, Error, Mode, SearchRequest};
use vectordb::distance::Metric;
use vectordb::filter::{Filter, Metadata, Value};
use vectordb::wal::SyncMode;

fn cfg(dim: usize) -> Config {
    Config { dim, metric: Metric::L2, m: 8, ef_construction: 64, seed: 3 }
}

fn vec_for(i: u64, dim: usize) -> Vec<f32> {
    let mut r = SmallRng::seed_from_u64(i);
    (0..dim).map(|_| r.gen_range(-1.0..1.0)).collect()
}

fn meta(i: u64) -> Metadata {
    let mut m = BTreeMap::new();
    m.insert("group".to_string(), Value::Int((i % 10) as i64));
    m.insert("even".to_string(), Value::Bool(i.is_multiple_of(2)));
    m
}

fn req(v: Vec<f32>, k: usize) -> SearchRequest {
    SearchRequest { vector: v, k, ef: Some(64), filter: None, mode: Mode::Hnsw, rerank: None }
}

fn ids(c: &Collection, r: &SearchRequest) -> Vec<u64> {
    c.search(r).unwrap().hits.iter().map(|h| h.id).collect()
}

#[test]
fn basic_upsert_search_get_delete() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), cfg(8), SyncMode::Never).unwrap();
    for i in 0..200 {
        c.upsert(i, &vec_for(i, 8), Some(meta(i))).unwrap();
    }
    let hit = &c.search(&req(vec_for(5, 8), 1)).unwrap().hits[0];
    assert_eq!(hit.id, 5);
    assert_eq!(hit.distance, 0.0);
    assert_eq!(c.get(5).unwrap().1.unwrap()["group"], Value::Int(5));
    // upsert replaces
    c.upsert(5, &vec_for(1000, 8), None).unwrap();
    assert_eq!(c.search(&req(vec_for(1000, 8), 1)).unwrap().hits[0].id, 5);
    assert_ne!(ids(&c, &req(vec_for(5, 8), 1)), vec![5]);
    assert_eq!(c.stats().live, 200);
    assert!(c.delete(5).unwrap());
    assert!(!c.delete(5).unwrap());
    assert!(!ids(&c, &req(vec_for(1000, 8), 50)).contains(&5));
    assert!(c.get(5).is_none());
    c.check().unwrap();
}

#[test]
fn input_validation() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), Config::new(4, Metric::Cosine), SyncMode::Never).unwrap();
    assert!(c.upsert(1, &[1.0, 2.0], None).is_err());
    assert!(c.upsert(1, &[f32::NAN, 0.0, 0.0, 0.0], None).is_err());
    assert!(c.upsert(1, &[0.0; 4], None).is_err()); // zero vector under cosine
    c.upsert(1, &[1.0, 0.0, 0.0, 0.0], None).unwrap();
    assert!(c.search(&req(vec![1.0, 0.0, 0.0, 0.0], 0)).is_err());
    assert!(c.search(&SearchRequest { ef: Some(1_000_000), ..req(vec![1.0, 0.0, 0.0, 0.0], 1) }).is_err());
    assert!(c.search(&SearchRequest { mode: Mode::Pq, ..req(vec![1.0, 0.0, 0.0, 0.0], 1) }).is_err());
    // nothing invalid reached the WAL
    assert_eq!(c.stats().lsn, 1);
    // a second handle on the same directory is refused while the first is open
    assert!(matches!(Collection::open(d.path(), Config::new(4, Metric::Cosine), SyncMode::Never), Err(Error::Locked(_))));
    drop(c);
    assert!(matches!(Collection::open(d.path(), Config::new(5, Metric::Cosine), SyncMode::Never), Err(Error::Config(_))));
    assert_eq!(Collection::open(d.path(), Config::new(4, Metric::Cosine), SyncMode::Never).unwrap().stats().live, 1);
}

#[test]
fn recovery_from_wal_and_snapshot_gives_identical_results() {
    let d = tempfile::tempdir().unwrap();
    let queries: Vec<_> = (0..20).map(|i| req(vec_for(10_000 + i, 16), 10)).collect();
    let before: Vec<Vec<u64>>;
    {
        let c = Collection::open(d.path(), cfg(16), SyncMode::Always).unwrap();
        for i in 0..300 {
            c.upsert(i, &vec_for(i, 16), Some(meta(i))).unwrap();
        }
        c.snapshot().unwrap();
        for i in 300..500 {
            c.upsert(i, &vec_for(i, 16), Some(meta(i))).unwrap();
        }
        for i in (0..500).step_by(7) {
            c.delete(i).unwrap();
        }
        before = queries.iter().map(|q| ids(&c, q)).collect();
    }
    let c = Collection::open(d.path(), cfg(16), SyncMode::Always).unwrap();
    assert_eq!(c.recovered.snapshot_lsn, Some(300));
    assert_eq!(c.recovered.wal_records_replayed, 200 + 72);
    c.check().unwrap();
    let after: Vec<Vec<u64>> = queries.iter().map(|q| ids(&c, q)).collect();
    assert_eq!(before, after);
}

#[test]
fn crash_at_every_wal_byte_recovers_exactly_the_acknowledged_prefix() {
    // Model-based: apply a random op sequence, record the WAL size after each
    // acknowledged op, then reopen copies of the directory with the WAL cut at
    // many offsets. The recovered live set must equal the model after the last
    // op whose record is completely inside the cut.
    let d = tempfile::tempdir().unwrap();
    let mut rng = SmallRng::seed_from_u64(11);
    let mut models: Vec<HashMap<u64, Vec<f32>>> = vec![HashMap::new()];
    let mut sizes = vec![8u64];
    {
        let c = Collection::open(d.path(), cfg(4), SyncMode::Never).unwrap();
        let mut m: HashMap<u64, Vec<f32>> = HashMap::new();
        for step in 0..60u64 {
            let id = rng.gen_range(0..15u64);
            if rng.gen_bool(0.25) {
                if c.delete(id).unwrap() {
                    m.remove(&id);
                } else {
                    continue;
                }
            } else {
                let v = vec_for(step * 100 + id, 4);
                c.upsert(id, &v, None).unwrap();
                m.insert(id, v);
            }
            models.push(m.clone());
            sizes.push(c.stats().wal_bytes);
        }
    }
    let wal = std::fs::read(d.path().join("wal.log")).unwrap();
    let cfgb = std::fs::read(d.path().join("config.json")).unwrap();
    let mut cuts: Vec<usize> = (0..wal.len()).step_by(7).collect();
    cuts.extend(sizes.iter().map(|&s| s as usize));
    cuts.extend(sizes.iter().map(|&s| s as usize - 1));
    for cut in cuts.into_iter().filter(|&c| c <= wal.len()) {
        let e = tempfile::tempdir().unwrap();
        std::fs::write(e.path().join("wal.log"), &wal[..cut]).unwrap();
        std::fs::write(e.path().join("config.json"), &cfgb).unwrap();
        let c = Collection::open(e.path(), cfg(4), SyncMode::Never).unwrap_or_else(|err| panic!("cut {cut}: {err}"));
        let k = sizes.iter().filter(|&&s| s as usize <= cut).count().saturating_sub(1); // cut < 8: torn magic, empty
        let want = &models[k];
        assert_eq!(c.stats().live, want.len(), "cut {cut}");
        for (id, v) in want {
            assert_eq!(&c.get(*id).unwrap().0, v, "cut {cut} id {id}");
        }
        c.check().unwrap();
    }
}

#[test]
fn corrupt_snapshot_fails_open() {
    let d = tempfile::tempdir().unwrap();
    {
        let c = Collection::open(d.path(), cfg(4), SyncMode::Never).unwrap();
        for i in 0..50 {
            c.upsert(i, &vec_for(i, 4), None).unwrap();
        }
        c.snapshot().unwrap();
    }
    let p = d.path().join("snapshot.vdb");
    let mut b = std::fs::read(&p).unwrap();
    let mid = b.len() / 2;
    b[mid] ^= 1;
    std::fs::write(&p, &b).unwrap();
    assert!(Collection::open(d.path(), cfg(4), SyncMode::Never).is_err());
}

#[test]
fn rebuild_compacts_and_preserves_results() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), cfg(16), SyncMode::Never).unwrap();
    for i in 0..1000 {
        c.upsert(i, &vec_for(i, 16), Some(meta(i))).unwrap();
    }
    for i in 0..500 {
        c.delete(i * 2).unwrap();
    }
    let q = SearchRequest { mode: Mode::Exact, ..req(vec_for(77_777, 16), 10) };
    let before = ids(&c, &q);
    let s = c.rebuild().unwrap();
    assert_eq!((s.live, s.slots, s.deleted_fraction), (500, 500, 0.0));
    assert_eq!(ids(&c, &q), before);
    c.check().unwrap();
    drop(c);
    let c = Collection::open(d.path(), cfg(16), SyncMode::Never).unwrap();
    assert_eq!(ids(&c, &q), before);
}

#[test]
fn pq_search_after_training_and_persistence() {
    let d = tempfile::tempdir().unwrap();
    let c = Collection::open(d.path(), cfg(16), SyncMode::Never).unwrap();
    for i in 0..1500 {
        c.upsert(i, &vec_for(i, 16), None).unwrap();
    }
    c.train_pq(4, 1500).unwrap();
    c.upsert(5000, &vec_for(5000, 16), None).unwrap(); // encoded with the trained codebooks
    let q = SearchRequest { mode: Mode::Pq, rerank: Some(20), ..req(vec_for(5000, 16), 5) };
    assert_eq!(c.search(&q).unwrap().hits[0].id, 5000);
    drop(c);
    let c = Collection::open(d.path(), cfg(16), SyncMode::Never).unwrap();
    assert_eq!(c.search(&q).unwrap().hits[0].id, 5000);
    c.check().unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// Filtered search never returns a non-matching or deleted item, returns
    /// as many hits as exist (up to k), and in exact mode equals brute force
    /// over the matching items.
    #[test]
    fn filtered_search_contract(n in 50u64..400, group in 0i64..10, k in 1usize..30, del_every in 2u64..9, seed in 0u64..1000) {
        let d = tempfile::tempdir().unwrap();
        let c = Collection::open(d.path(), cfg(8), SyncMode::Never).unwrap();
        for i in 0..n {
            c.upsert(i, &vec_for(i ^ seed, 8), Some(meta(i))).unwrap();
        }
        for i in (0..n).step_by(del_every as usize) {
            c.delete(i).unwrap();
        }
        let f = Filter::Eq("group".into(), Value::Int(group));
        let matching: HashSet<u64> = (0..n).filter(|i| i % 10 == group as u64 && i % del_every != 0).collect();
        let q = vec_for(seed + 99_999, 8);
        for mode in [Mode::Hnsw, Mode::Exact] {
            let r = c.search(&SearchRequest { vector: q.clone(), k, ef: Some(64), filter: Some(f.clone()), mode, rerank: None }).unwrap();
            prop_assert!(r.hits.iter().all(|h| matching.contains(&h.id)), "{mode:?} returned a non-matching id");
            prop_assert_eq!(r.hits.len(), k.min(matching.len()));
            prop_assert!(r.hits.windows(2).all(|w| w[0].distance <= w[1].distance));
            if mode == Mode::Exact {
                let mut truth: Vec<(f32, u64)> = matching.iter().map(|&i| (Metric::L2.distance(&q, &c.get(i).unwrap().0), i)).collect();
                truth.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                let want: Vec<u64> = truth.iter().take(k).map(|x| x.1).collect();
                prop_assert_eq!(r.hits.iter().map(|h| h.id).collect::<Vec<_>>(), want);
            }
        }
    }
}

#[test]
fn concurrent_writers_and_readers() {
    let d = tempfile::tempdir().unwrap();
    let c = Arc::new(Collection::open(d.path(), cfg(16), SyncMode::Never).unwrap());
    let mut hs = Vec::new();
    for t in 0..4u64 {
        let c = c.clone();
        hs.push(std::thread::spawn(move || {
            for i in 0..500u64 {
                let id = t * 10_000 + i;
                c.upsert(id, &vec_for(id, 16), Some(meta(id))).unwrap();
                if i % 5 == 0 {
                    c.delete(id).unwrap();
                }
            }
        }));
    }
    for t in 0..4u64 {
        let c = c.clone();
        hs.push(std::thread::spawn(move || {
            for i in 0..400u64 {
                let r = c.search(&req(vec_for(t * 777 + i, 16), 10)).unwrap();
                assert!(r.hits.windows(2).all(|w| w[0].distance <= w[1].distance));
                for h in &r.hits {
                    assert!(h.id % 10_000 < 500 && h.id / 10_000 < 4);
                }
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(c.stats().live, 4 * 400);
    c.check().unwrap();
    for t in 0..4u64 {
        for i in 0..500u64 {
            let id = t * 10_000 + i;
            assert_eq!(c.get(id).is_some(), i % 5 != 0, "id {id}");
        }
    }
}
