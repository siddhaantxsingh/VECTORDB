//! Hierarchical Navigable Small World graph (Malkov & Yashunin, 2018),
//! implemented from scratch.
//!
//! * level of a new node: floor(-ln(U) * mL), mL = 1 / ln(M), capped at 16
//! * insertion: greedy descent (ef = 1) to the node's top level, then on each
//!   level a beam search with ef_construction and neighbour selection by the
//!   paper's heuristic (Algorithm 4, keepPrunedConnections); back-links are
//!   added and over-full neighbour lists are re-pruned with the heuristic
//! * layer 0 keeps up to 2M links, upper layers M
//! * search: greedy descent, then a beam search with ef = max(ef, k) on layer
//!   0. An optional `allow` predicate (tombstones, metadata filters) is
//!   applied INSIDE the beam search: disallowed nodes are traversed but never
//!   enter the result set.
//!
//! Invariants checked by `check_invariants` (used in tests): link lists have
//! no self-loops or duplicates, respect the per-level capacity, only point to
//! nodes that exist on that level, and the entry point has the maximum level.

use crate::store::VectorStore;
use parking_lot::Mutex;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

pub const MAX_LEVEL: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HnswParams {
    pub m: usize,
    pub ef_construction: usize,
    pub seed: u64,
}

impl Default for HnswParams {
    fn default() -> Self {
        HnswParams { m: 16, ef_construction: 200, seed: 42 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cand(f32, u32);
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0).then(self.1.cmp(&o.1))
    }
}

struct Visited {
    marks: Vec<u32>,
    gen: u32,
}

impl Visited {
    fn reset(&mut self, n: usize) {
        if self.marks.len() < n {
            self.marks.resize(n, 0);
        }
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0 {
            self.marks.iter_mut().for_each(|m| *m = 0);
            self.gen = 1;
        }
    }
    #[inline]
    fn insert(&mut self, i: u32) -> bool {
        let m = &mut self.marks[i as usize];
        if *m == self.gen {
            false
        } else {
            *m = self.gen;
            true
        }
    }
}

pub struct Hnsw {
    pub params: HnswParams,
    pub levels: Vec<u8>,
    pub links: Vec<Vec<Vec<u32>>>, // links[node][level]
    pub entry: Option<u32>,
    pub max_level: usize,
    ml: f64,
    rng: SmallRng,
    pool: Mutex<Vec<Visited>>,
}

pub type Allow<'a> = Option<&'a (dyn Fn(u32) -> bool + Sync)>;

impl Hnsw {
    pub fn new(params: HnswParams) -> Self {
        assert!(params.m >= 2, "M must be >= 2");
        Hnsw {
            params,
            levels: Vec::new(),
            links: Vec::new(),
            entry: None,
            max_level: 0,
            ml: 1.0 / (params.m as f64).ln(),
            rng: SmallRng::seed_from_u64(params.seed),
            pool: Mutex::new(Vec::new()),
        }
    }

    pub fn len(&self) -> usize {
        self.levels.len()
    }

    /// Heap bytes of the adjacency lists (capacity, plus `Vec` headers) and levels.
    pub fn memory_bytes(&self) -> usize {
        let hdr = std::mem::size_of::<Vec<u32>>();
        self.levels.capacity() + self.links.iter().map(|l| hdr + l.iter().map(|x| hdr + x.capacity() * 4).sum::<usize>()).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    fn cap(&self, level: usize) -> usize {
        if level == 0 {
            2 * self.params.m
        } else {
            self.params.m
        }
    }

    fn visited(&self, n: usize) -> Visited {
        let mut v = self.pool.lock().pop().unwrap_or(Visited { marks: Vec::new(), gen: 0 });
        v.reset(n);
        v
    }

    fn give_back(&self, v: Visited) {
        let mut p = self.pool.lock();
        if p.len() < 64 {
            p.push(v);
        }
    }

    fn random_level(&mut self) -> usize {
        let u: f64 = 1.0 - self.rng.gen::<f64>(); // (0, 1]
        ((-u.ln() * self.ml).floor() as usize).min(MAX_LEVEL)
    }

    /// Beam search on one level. Returns up to `ef` allowed nodes, closest first.
    fn search_layer(&self, store: &VectorStore, q: &[f32], eps: &[Cand], ef: usize, level: usize, allow: Allow) -> Vec<Cand> {
        let mut vis = self.visited(store.len());
        let mut cands: BinaryHeap<Reverse<Cand>> = BinaryHeap::new();
        let mut res: BinaryHeap<Cand> = BinaryHeap::new();
        for &e in eps {
            if vis.insert(e.1) {
                cands.push(Reverse(e));
                if allow.is_none_or(|f| f(e.1)) {
                    res.push(e);
                }
            }
        }
        while res.len() > ef {
            res.pop();
        }
        while let Some(Reverse(c)) = cands.pop() {
            if res.len() >= ef && c.0 > res.peek().unwrap().0 {
                break;
            }
            for &n in &self.links[c.1 as usize][level] {
                if !vis.insert(n) {
                    continue;
                }
                let d = store.distance(q, n);
                if res.len() < ef || d < res.peek().unwrap().0 {
                    cands.push(Reverse(Cand(d, n)));
                    if allow.is_none_or(|f| f(n)) {
                        res.push(Cand(d, n));
                        if res.len() > ef {
                            res.pop();
                        }
                    }
                }
            }
        }
        self.give_back(vis);
        let mut v = res.into_vec();
        v.sort();
        v
    }

    /// Neighbour selection heuristic: keep a candidate only if it is closer
    /// to the base than to every neighbour already kept; then fill up with the
    /// pruned candidates (keepPrunedConnections) to `m`.
    fn select(&self, store: &VectorStore, cands: &[Cand], m: usize) -> Vec<u32> {
        let mut kept: Vec<Cand> = Vec::with_capacity(m);
        let mut pruned: Vec<Cand> = Vec::new();
        for &c in cands {
            if kept.len() >= m {
                break;
            }
            let cv = store.vector(c.1);
            if kept.iter().all(|k| store.metric.distance(cv, store.vector(k.1)) > c.0) {
                kept.push(c);
            } else {
                pruned.push(c);
            }
        }
        for c in pruned {
            if kept.len() >= m {
                break;
            }
            kept.push(c);
        }
        kept.into_iter().map(|c| c.1).collect()
    }

    /// Inserts slot `id` (which must be the next slot: id == self.len()).
    pub fn insert(&mut self, store: &VectorStore, id: u32) {
        assert_eq!(id as usize, self.levels.len(), "HNSW slots must be inserted in order");
        let level = self.random_level();
        self.levels.push(level as u8);
        self.links.push(vec![Vec::new(); level + 1]);
        let Some(entry) = self.entry else {
            self.entry = Some(id);
            self.max_level = level;
            return;
        };
        let q = store.vector(id).to_vec();
        let mut eps = vec![Cand(store.distance(&q, entry), entry)];
        for l in (level + 1..=self.max_level).rev() {
            eps = self.search_layer(store, &q, &eps, 1, l, None);
        }
        for l in (0..=level.min(self.max_level)).rev() {
            let w = self.search_layer(store, &q, &eps, self.params.ef_construction, l, None);
            let neigh = self.select(store, &w, self.params.m);
            let cap = self.cap(l);
            for &n in &neigh {
                let list = &mut self.links[n as usize][l];
                list.push(id);
                if list.len() > cap {
                    let base = store.vector(n);
                    let mut cs: Vec<Cand> = list.iter().map(|&x| Cand(store.metric.distance(base, store.vector(x)), x)).collect();
                    cs.sort();
                    let pruned = self.select(store, &cs, cap);
                    self.links[n as usize][l] = pruned;
                }
            }
            self.links[id as usize][l] = neigh;
            eps = w;
        }
        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(id);
        }
    }

    /// k nearest allowed nodes, closest first.
    pub fn search(&self, store: &VectorStore, q: &[f32], k: usize, ef: usize, allow: Allow) -> Vec<(f32, u32)> {
        let Some(entry) = self.entry else { return Vec::new() };
        let mut eps = vec![Cand(store.distance(q, entry), entry)];
        for l in (1..=self.max_level).rev() {
            eps = self.search_layer(store, q, &eps, 1, l, None);
        }
        let mut res = self.search_layer(store, q, &eps, ef.max(k), 0, allow);
        res.truncate(k);
        res.into_iter().map(|c| (c.0, c.1)).collect()
    }

    /// Structural invariants (for tests and `vectordb check`).
    pub fn check_invariants(&self) -> Result<(), String> {
        let n = self.levels.len();
        if n == 0 {
            return if self.entry.is_none() { Ok(()) } else { Err("empty graph with entry point".into()) };
        }
        let e = self.entry.ok_or("non-empty graph without entry point")? as usize;
        if self.levels[e] as usize != self.max_level || self.levels.iter().any(|&l| l as usize > self.max_level) {
            return Err("entry point is not on the top level".into());
        }
        for (i, per) in self.links.iter().enumerate() {
            if per.len() != self.levels[i] as usize + 1 {
                return Err(format!("node {i}: {} link levels for level {}", per.len(), self.levels[i]));
            }
            for (l, list) in per.iter().enumerate() {
                if list.len() > self.cap(l) {
                    return Err(format!("node {i} level {l}: {} links > cap {}", list.len(), self.cap(l)));
                }
                let mut seen = std::collections::HashSet::new();
                for &x in list {
                    if x as usize == i {
                        return Err(format!("node {i} level {l}: self-loop"));
                    }
                    if x as usize >= n || (self.levels[x as usize] as usize) < l {
                        return Err(format!("node {i} level {l}: link to {x} which is not on level {l}"));
                    }
                    if !seen.insert(x) {
                        return Err(format!("node {i} level {l}: duplicate link {x}"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Restores a graph from persisted parts (snapshot load).
    pub fn from_parts(params: HnswParams, levels: Vec<u8>, links: Vec<Vec<Vec<u32>>>, entry: Option<u32>, max_level: usize, rng_seed: u64) -> Self {
        let mut h = Hnsw::new(params);
        h.levels = levels;
        h.links = links;
        h.entry = entry;
        h.max_level = max_level;
        h.rng = SmallRng::seed_from_u64(rng_seed);
        h
    }
}

/// Exact k-NN by full scan (the ground truth for recall measurements and the
/// fallback for very selective filters).
pub fn exact_search(store: &VectorStore, q: &[f32], k: usize, allow: Allow) -> Vec<(f32, u32)> {
    let mut heap: BinaryHeap<Cand> = BinaryHeap::with_capacity(k + 1);
    for i in 0..store.len() as u32 {
        if !allow.is_none_or(|f| f(i)) {
            continue;
        }
        let d = store.distance(q, i);
        if heap.len() < k {
            heap.push(Cand(d, i));
        } else if let Some(top) = heap.peek() {
            if Cand(d, i) < *top {
                heap.pop();
                heap.push(Cand(d, i));
            }
        }
    }
    let mut v = heap.into_vec();
    v.sort();
    v.into_iter().map(|c| (c.0, c.1)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::distance::Metric;

    fn random_store(n: usize, dim: usize, seed: u64, metric: Metric) -> VectorStore {
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut s = VectorStore::new(dim, metric);
        for i in 0..n {
            let v: Vec<f32> = (0..dim).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let v = s.prepare(&v).unwrap();
            s.push(i as u64, &v, None).unwrap();
        }
        s
    }

    fn recall(store: &VectorStore, h: &Hnsw, queries: usize, k: usize, ef: usize) -> f64 {
        let mut rng = SmallRng::seed_from_u64(99);
        let mut hit = 0;
        for _ in 0..queries {
            let q: Vec<f32> = (0..store.dim).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let q = store.prepare(&q).unwrap();
            let truth: std::collections::HashSet<u32> = exact_search(store, &q, k, None).into_iter().map(|x| x.1).collect();
            hit += h.search(store, &q, k, ef, None).iter().filter(|x| truth.contains(&x.1)).count();
        }
        hit as f64 / (queries * k) as f64
    }

    #[test]
    fn recall_is_high_and_invariants_hold() {
        for metric in [Metric::L2, Metric::Cosine, Metric::Ip] {
            let s = random_store(3000, 32, 7, metric);
            let mut h = Hnsw::new(HnswParams { m: 12, ef_construction: 100, seed: 1 });
            for i in 0..s.len() as u32 {
                h.insert(&s, i);
            }
            h.check_invariants().unwrap();
            let r = recall(&s, &h, 100, 10, 100);
            assert!(r >= 0.95, "{metric:?} recall@10 = {r}");
        }
    }

    #[test]
    fn deterministic_for_a_seed() {
        let s = random_store(500, 8, 3, Metric::L2);
        let build = || {
            let mut h = Hnsw::new(HnswParams { m: 8, ef_construction: 50, seed: 9 });
            for i in 0..s.len() as u32 {
                h.insert(&s, i);
            }
            h
        };
        let (a, b) = (build(), build());
        assert_eq!(a.links, b.links);
        assert_eq!(a.entry, b.entry);
    }

    #[test]
    fn allow_predicate_is_never_violated_and_results_are_full() {
        let s = random_store(2000, 16, 5, Metric::L2);
        let mut h = Hnsw::new(HnswParams { m: 8, ef_construction: 64, seed: 2 });
        for i in 0..s.len() as u32 {
            h.insert(&s, i);
        }
        let only_even = |i: u32| i.is_multiple_of(2);
        let q = s.vector(1).to_vec();
        let r = h.search(&s, &q, 20, 64, Some(&only_even));
        assert_eq!(r.len(), 20);
        assert!(r.iter().all(|x| x.1 % 2 == 0));
        assert!(r.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn empty_and_single() {
        let mut s = VectorStore::new(4, Metric::L2);
        let h = Hnsw::new(HnswParams::default());
        assert!(h.search(&s, &[0.0; 4], 5, 10, None).is_empty());
        s.push(1, &[1.0, 0.0, 0.0, 0.0], None).unwrap();
        let mut h = Hnsw::new(HnswParams::default());
        h.insert(&s, 0);
        assert_eq!(h.search(&s, &[0.0; 4], 5, 10, None), vec![(1.0, 0)]);
        h.check_invariants().unwrap();
    }
}
