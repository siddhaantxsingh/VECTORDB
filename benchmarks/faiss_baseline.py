"""FAISS IndexHNSWFlat baseline on the exact dataset vdb-bench wrote.

    python3 benchmarks/faiss_baseline.py BENCH_OUT_DIR [--m 16] [--ef-construction 200]

Reads base.fvecs / query.fvecs / groundtruth.ivecs from BENCH_OUT_DIR (the
ground truth is vectordb's exact search, which is brute force and therefore
engine-independent), builds IndexHNSWFlat with the same M / efConstruction,
and measures recall@10, QPS and per-query latency for the same ef values on
1 and 2 threads. Per-query latency is measured with single-query calls, the
same way vdb-bench issues requests; QPS on N threads uses one batched call
with omp_set_num_threads(N). Writes BENCH_OUT_DIR/faiss_results.json.
"""
import argparse
import json
import time

import faiss
import numpy as np


def read_vecs(path, dtype):
    raw = np.fromfile(path, dtype=np.int32)
    d = raw[0]
    rows = raw.reshape(-1, d + 1)[:, 1:]
    return np.ascontiguousarray(rows.view(dtype) if dtype == np.float32 else rows)


def recall(found, truth, k=10):
    return float(np.mean([len(set(f[:k]) & set(t[:k])) / k for f, t in zip(found, truth)]))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--m", type=int, default=16)
    ap.add_argument("--ef-construction", type=int, default=200)
    a = ap.parse_args()
    xb = read_vecs(f"{a.dir}/base.fvecs", np.float32)
    xq = read_vecs(f"{a.dir}/query.fvecs", np.float32)
    gt = read_vecs(f"{a.dir}/groundtruth.ivecs", np.int32)
    k = 10
    out = {"setup": {"faiss": faiss.__version__, "index": "IndexHNSWFlat", "n": int(xb.shape[0]), "dim": int(xb.shape[1]),
                     "queries": int(xq.shape[0]), "m": a.m, "ef_construction": a.ef_construction}}

    faiss.omp_set_num_threads(1)
    idx = faiss.IndexHNSWFlat(xb.shape[1], a.m)
    idx.hnsw.efConstruction = a.ef_construction
    t0 = time.perf_counter()
    idx.add(xb)
    build = time.perf_counter() - t0
    out["build_threads_1"] = {"seconds": round(build, 2), "inserts_per_sec": round(xb.shape[0] / build)}

    sweep = []
    for ef in [10, 16, 32, 64, 128, 256]:
        idx.hnsw.efSearch = ef
        row = {"ef": ef}
        for threads in (1, 2):
            faiss.omp_set_num_threads(threads)
            idx.search(xq[:100], k)  # warm-up
            t0 = time.perf_counter()
            _, ids = idx.search(xq, k)
            wall = time.perf_counter() - t0
            row[f"threads_{threads}_batched"] = {"recall_at_10": round(recall(ids, gt), 4), "qps": round(xq.shape[0] / wall)}
        faiss.omp_set_num_threads(1)
        lat, found = [], []
        for q in xq:
            t0 = time.perf_counter()
            _, i = idx.search(q[None, :], k)
            lat.append((time.perf_counter() - t0) * 1e6)
            found.append(i[0])
        lat.sort()
        pick = lambda p: round(lat[round((len(lat) - 1) * p)])
        row["threads_1_single_query"] = {"recall_at_10": round(recall(found, gt), 4), "qps": round(len(lat) / (sum(lat) / 1e6)),
                                         "p50_us": pick(0.5), "p95_us": pick(0.95), "p99_us": pick(0.99)}
        sweep.append(row)
        print(json.dumps(row))
    out["hnsw"] = sweep
    with open(f"{a.dir}/faiss_results.json", "w") as f:
        json.dump(out, f, indent=2)


if __name__ == "__main__":
    main()
