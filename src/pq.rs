//! Product quantization (Jégou, Douze, Schmid 2011), from scratch.
//!
//! The d-dimensional space is split into `m` sub-spaces of d/m dimensions;
//! each gets a codebook of up to 256 centroids trained by k-means
//! (k-means++ seeding, Lloyd iterations, fixed seed). A vector is stored as
//! `m` bytes. Queries use asymmetric distance computation (ADC): a lookup
//! table of query-to-centroid partial distances per sub-space, summed over a
//! vector's codes. `search` scans all codes with ADC, keeps the best
//! `k * rerank` candidates, and re-ranks those with exact distances on the
//! raw vectors (so the final ranking is exact among the candidates).
//!
//! Distances decompose over sub-spaces for L2 (sum of squared differences)
//! and for inner product / cosine (sum of partial dot products).

use crate::distance::{dot_scalar, l2_sq_scalar, Metric};
use crate::store::VectorStore;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::BinaryHeap;

#[derive(Clone, Debug)]
pub struct Pq {
    pub m: usize,
    pub ksub: usize,
    pub dsub: usize,
    pub metric: Metric,
    pub centroids: Vec<f32>, // [m][ksub][dsub]
    pub codes: Vec<u8>,      // [n][m], aligned with store slots
}

fn kmeans(data: &[f32], dsub: usize, k: usize, iters: usize, rng: &mut SmallRng) -> Vec<f32> {
    let n = data.len() / dsub;
    let pt = |i: usize| &data[i * dsub..(i + 1) * dsub];
    // k-means++ seeding
    let mut cents: Vec<f32> = Vec::with_capacity(k * dsub);
    cents.extend_from_slice(pt(rng.gen_range(0..n)));
    let mut best = vec![f32::INFINITY; n];
    for c in 1..k {
        let last = &cents[(c - 1) * dsub..c * dsub];
        let mut total = 0.0f64;
        for i in 0..n {
            best[i] = best[i].min(l2_sq_scalar(pt(i), last));
            total += best[i] as f64;
        }
        let pick = if total <= 0.0 {
            rng.gen_range(0..n)
        } else {
            let mut r = rng.gen::<f64>() * total;
            let mut chosen = n - 1;
            for (i, b) in best.iter().enumerate() {
                r -= *b as f64;
                if r <= 0.0 {
                    chosen = i;
                    break;
                }
            }
            chosen
        };
        cents.extend_from_slice(pt(pick));
    }
    // Lloyd
    let mut assign = vec![0usize; n];
    for _ in 0..iters {
        let mut changed = false;
        for i in 0..n {
            let p = pt(i);
            let (mut bi, mut bd) = (0, f32::INFINITY);
            for c in 0..k {
                let d = l2_sq_scalar(p, &cents[c * dsub..(c + 1) * dsub]);
                if d < bd {
                    bd = d;
                    bi = c;
                }
            }
            if assign[i] != bi {
                assign[i] = bi;
                changed = true;
            }
        }
        let mut sums = vec![0.0f64; k * dsub];
        let mut counts = vec![0usize; k];
        for i in 0..n {
            counts[assign[i]] += 1;
            for (j, x) in pt(i).iter().enumerate() {
                sums[assign[i] * dsub + j] += *x as f64;
            }
        }
        for c in 0..k {
            if counts[c] == 0 {
                // empty cluster: re-seed with a random point
                let r = rng.gen_range(0..n);
                cents[c * dsub..(c + 1) * dsub].copy_from_slice(pt(r));
                continue;
            }
            for j in 0..dsub {
                cents[c * dsub + j] = (sums[c * dsub + j] / counts[c] as f64) as f32;
            }
        }
        if !changed {
            break;
        }
    }
    cents
}

impl Pq {
    /// Trains codebooks on (a sample of up to `max_train`) live vectors and
    /// encodes every slot of the store.
    pub fn train(store: &VectorStore, m: usize, max_train: usize, iters: usize, seed: u64) -> Result<Pq, String> {
        if m == 0 || !store.dim.is_multiple_of(m) {
            return Err(format!("PQ needs m dividing the dimension ({} % {m} != 0)", store.dim));
        }
        let live: Vec<u32> = (0..store.len() as u32).filter(|&i| !store.deleted[i as usize]).collect();
        if live.len() < 2 {
            return Err("PQ needs at least 2 vectors to train".into());
        }
        if max_train < 2 {
            return Err("max_train must be at least 2".into());
        }
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut sample = live.clone();
        if sample.len() > max_train {
            for i in 0..max_train {
                let j = rng.gen_range(i..sample.len());
                sample.swap(i, j);
            }
            sample.truncate(max_train);
        }
        let dsub = store.dim / m;
        let ksub = 256.min(sample.len());
        let mut centroids = Vec::with_capacity(m * ksub * dsub);
        for sub in 0..m {
            let mut data = Vec::with_capacity(sample.len() * dsub);
            for &i in &sample {
                data.extend_from_slice(&store.vector(i)[sub * dsub..(sub + 1) * dsub]);
            }
            centroids.extend(kmeans(&data, dsub, ksub, iters, &mut rng));
        }
        let mut pq = Pq { m, ksub, dsub, metric: store.metric, centroids, codes: Vec::new() };
        for i in 0..store.len() as u32 {
            pq.push(store.vector(i));
        }
        Ok(pq)
    }

    fn centroid(&self, sub: usize, c: usize) -> &[f32] {
        let s = (sub * self.ksub + c) * self.dsub;
        &self.centroids[s..s + self.dsub]
    }

    pub fn encode(&self, v: &[f32]) -> Vec<u8> {
        (0..self.m)
            .map(|sub| {
                let x = &v[sub * self.dsub..(sub + 1) * self.dsub];
                (0..self.ksub).min_by(|&a, &b| l2_sq_scalar(x, self.centroid(sub, a)).total_cmp(&l2_sq_scalar(x, self.centroid(sub, b)))).unwrap() as u8
            })
            .collect()
    }

    /// Encodes the next store slot (keeps codes aligned with slots).
    pub fn push(&mut self, v: &[f32]) {
        let c = self.encode(v);
        self.codes.extend(c);
    }

    pub fn decode(&self, slot: u32) -> Vec<f32> {
        let code = &self.codes[slot as usize * self.m..(slot as usize + 1) * self.m];
        code.iter().enumerate().flat_map(|(sub, &c)| self.centroid(sub, c as usize).to_vec()).collect()
    }

    fn lut(&self, q: &[f32]) -> Vec<f32> {
        let mut t = vec![0.0f32; self.m * self.ksub];
        for sub in 0..self.m {
            let x = &q[sub * self.dsub..(sub + 1) * self.dsub];
            for c in 0..self.ksub {
                let cent = self.centroid(sub, c);
                t[sub * self.ksub + c] = match self.metric {
                    Metric::L2 => l2_sq_scalar(x, cent),
                    Metric::Ip | Metric::Cosine => -dot_scalar(x, cent),
                };
            }
        }
        t
    }

    /// Approximate distance of `slot` to the query whose table is `lut`.
    #[inline]
    fn adc(&self, lut: &[f32], slot: usize) -> f32 {
        let code = &self.codes[slot * self.m..(slot + 1) * self.m];
        let mut s = 0.0;
        for (sub, &c) in code.iter().enumerate() {
            s += lut[sub * self.ksub + c as usize];
        }
        if self.metric == Metric::Cosine {
            s + 1.0
        } else {
            s
        }
    }

    pub fn search(&self, store: &VectorStore, q: &[f32], k: usize, rerank: usize, allow: crate::hnsw::Allow) -> Vec<(f32, u32)> {
        #[derive(PartialEq)]
        struct C(f32, u32);
        impl Eq for C {}
        impl PartialOrd for C {
            fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(o))
            }
        }
        impl Ord for C {
            fn cmp(&self, o: &Self) -> std::cmp::Ordering {
                self.0.total_cmp(&o.0).then(self.1.cmp(&o.1))
            }
        }
        let lut = self.lut(q);
        let r = (k * rerank.max(1)).max(k);
        let mut heap: BinaryHeap<C> = BinaryHeap::with_capacity(r + 1);
        for i in 0..self.codes.len() / self.m {
            if !allow.is_none_or(|f| f(i as u32)) {
                continue;
            }
            let d = self.adc(&lut, i);
            if heap.len() < r {
                heap.push(C(d, i as u32));
            } else if d < heap.peek().unwrap().0 {
                heap.pop();
                heap.push(C(d, i as u32));
            }
        }
        let mut out: Vec<(f32, u32)> = heap.into_iter().map(|c| (store.distance(q, c.1), c.1)).collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        out.truncate(k);
        out
    }

    pub fn memory_bytes(&self) -> usize {
        self.codes.len() + self.centroids.len() * 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hnsw::exact_search;

    fn clustered(n: usize, dim: usize, metric: Metric) -> VectorStore {
        let mut rng = SmallRng::seed_from_u64(4);
        let centers: Vec<Vec<f32>> = (0..20).map(|_| (0..dim).map(|_| rng.gen_range(-5.0..5.0)).collect()).collect();
        let mut s = VectorStore::new(dim, metric);
        for i in 0..n {
            let c = &centers[i % 20];
            let v: Vec<f32> = c.iter().map(|x| x + rng.gen_range(-1.0..1.0)).collect();
            let v = s.prepare(&v).unwrap();
            s.push(i as u64, &v, None).unwrap();
        }
        s
    }

    #[test]
    fn adc_equals_distance_to_reconstruction() {
        for metric in [Metric::L2, Metric::Ip, Metric::Cosine] {
            let s = clustered(600, 16, metric);
            let pq = Pq::train(&s, 4, 1000, 10, 1).unwrap();
            let q = s.vector(3).to_vec();
            let lut = pq.lut(&q);
            for i in [0usize, 17, 599] {
                let rec = pq.decode(i as u32);
                let exact = metric.distance(&q, &rec);
                assert!((pq.adc(&lut, i) - exact).abs() < 1e-3, "{metric:?}");
            }
        }
    }

    #[test]
    fn rerank_recall_is_high_and_memory_small() {
        let s = clustered(3000, 32, Metric::L2);
        let pq = Pq::train(&s, 8, 3000, 15, 2).unwrap();
        let mut hit = 0;
        for qi in 0..50u32 {
            let q = s.vector(qi * 7).to_vec();
            let truth: std::collections::HashSet<u32> = exact_search(&s, &q, 10, None).into_iter().map(|x| x.1).collect();
            hit += pq.search(&s, &q, 10, 10, None).iter().filter(|x| truth.contains(&x.1)).count();
        }
        assert!(hit as f64 / 500.0 >= 0.9, "recall {}", hit as f64 / 500.0);
        assert_eq!(pq.codes.len(), 3000 * 8); // 8 bytes per vector vs 128 raw
    }

    #[test]
    fn rejects_bad_m() {
        let s = clustered(10, 10, Metric::L2);
        assert!(Pq::train(&s, 3, 10, 2, 1).is_err());
    }
}
