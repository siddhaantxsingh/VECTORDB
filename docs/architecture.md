# Architecture

```
            REST (axum, :6333)            gRPC (tonic, :6334)
                    \                         /
                     ───────  Registry  ──────           name → Arc<Collection>, /metrics
                                 │
                       Collection (one per directory)
           ┌─────────────┬───────┴───────┬──────────────┐
         WAL         VectorStore        HNSW          PQ (optional)
     (wal.log)    vectors, ids,      graph over      codebooks + codes
                  tombstones,        store slots
                  metadata
                                 │
                       snapshot.vdb (all of the above at an LSN)
```

## Modules

| Module | Responsibility |
|---|---|
| `distance.rs` | L2 (squared), inner product (negated) and cosine distances. Scalar reference kernels plus AVX2+FMA kernels selected at runtime (`is_x86_feature_detected!`). Cosine vectors are normalised on insert, so cosine reduces to `1 - dot`. |
| `filter.rs` | Flat metadata (`string → bool / int / float / string`) and a filter algebra (`eq`, `in`, `range`, `and`, `or`, `not`) with a depth limit (16), at most 1024 values per `in` list, and a budget of 2048 nodes plus `in` values per filter. |
| `store.rs` | Dense `u32` slots holding vectors, caller-chosen `u64` external ids, tombstones and metadata. Upserting an existing id tombstones the old slot and appends a new one. |
| `hnsw.rs` | HNSW from scratch: random levels with `mL = 1/ln M`, greedy descent, beam search with `ef_construction`, the paper's neighbour-selection heuristic with `keepPrunedConnections`, `2M` links on layer 0. Filters are applied **inside** the beam search. `check_invariants` validates the graph. Also brute-force `exact_search`. |
| `pq.rs` | Product quantization: `m` sub-spaces × up to 256 centroids, k-means++ seeding and Lloyd iterations with a fixed seed, ADC lookup tables, exact rerank of the best `k × rerank` candidates. |
| `wal.rs` | Append-only log, records framed as `[len][crc32][payload]`. Torn or zero-filled tails are truncated on open; a checksum failure followed by more data is reported as corruption. |
| `snapshot.rs` | Whole-state snapshot with magic, version and trailing CRC32. Written to a temp file, fsynced, renamed, directory fsynced. Every length field is bounds-checked on decode. |
| `collection.rs` | Ties it together: write path, recovery, query planning, rebuild/compaction, PQ training, stats, `check()`. Holds an exclusive lock on `<dir>/LOCK`. |
| `server.rs` | Registry of collections under one data directory, REST and gRPC handlers, Prometheus text metrics. |
| `main.rs` | `vectordb serve` and `vectordb check`; graceful shutdown on SIGINT/SIGTERM. |
| `bin/bench.rs` | Benchmark harness (see [benchmarks.md](benchmarks.md)). |

## Write path

1. Validate the request (dimension, finite values, non-zero vector for cosine, metadata ≤ 64 KiB) **before** logging, so invalid input never reaches the WAL.
2. Take the WAL mutex; assign `lsn = current + 1`; append the record; `fsync` if `SyncMode::Always`.
3. Take the index write lock and apply the operation (store push + HNSW insert + PQ encode).
4. Acknowledge.

With `SyncMode::Always`, an acknowledged write is on disk. With `SyncMode::Never`, it survives a process crash but not a power loss. Writes are serialised by the WAL mutex; searches take the index read lock and only wait while step 3 runs (not during the fsync).

## Recovery

`Collection::open` takes the directory lock, checks `config.json` against the requested configuration, loads `snapshot.vdb` if present (failing on a bad checksum), then replays WAL records with `lsn > snapshot.lsn`. `snapshot()` writes the snapshot and then truncates the WAL; a crash between the two leaves records with `lsn ≤ snapshot.lsn`, which replay skips. HNSW level assignment is deterministic for an uninterrupted run; after a restart the level RNG is reseeded, so the graph after more inserts may differ from the one an uninterrupted run would have built (results are still valid; tested for equality only at the point of recovery).

## Query planning

| Request | Plan |
|---|---|
| `mode: exact` | brute force over allowed slots |
| `mode: pq` | ADC scan over all codes, exact rerank of the best `k × rerank` |
| `mode: hnsw`, no filter | beam search with `ef = max(ef, k)` (default `ef = max(64, k)`) |
| `mode: hnsw`, filter | estimate selectivity `s` from ≤ 512 sampled slots. If `s < 2 %`: exact search over matching slots. Otherwise HNSW with `ef' = ef / s` (capped at 10 000). |

The response always reports the plan that ran.

## Deletion and compaction

Deletes set a tombstone. Tombstoned nodes stay in the graph so that search can still navigate through them, but never enter results. `rebuild` copies live vectors into a fresh store, builds a new graph, re-encodes PQ codes with the existing codebooks, and snapshots.

## Server

* Collection names must match `[a-z0-9_-]{1,64}`, so a name can never escape the data directory.
* All index work runs on Tokio's blocking pool.
* REST body limit is 64 MiB and at most 10 000 points per upsert.
* `/metrics` exports per-collection live vectors, slots, queries, writes, WAL bytes and deleted fraction, plus a global search-latency histogram.
