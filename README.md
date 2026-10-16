# vectordb

A persistent approximate-nearest-neighbour search engine, written from scratch in Rust. It provides:

- an HNSW graph index;
- product quantization;
- AVX2/FMA distance kernels;
- metadata filters evaluated inside the graph search;
- a CRC-framed write-ahead log, checksummed snapshots, and crash recovery;
- REST (axum) and gRPC (tonic) front-ends with Prometheus metrics.

**Status.**
- **Done:** the engine, the server, the benchmarks and the audit are complete and verified locally. `cargo test` runs 41 tests and all pass, including:
  - a crash test that truncates the WAL at every byte;
  - a property test of filtered search;
  - end-to-end tests against a real server, including a restart;
  - WAL I/O fault injection (failed write, fsync and rollback, and a real EFBIG from the kernel): no acknowledged write is lost.
- **Docker:** the image builds (156 MB) and passes a smoke test, including a restart on the same volume (`scripts/docker_build.sh`, `scripts/docker_smoke.sh`).
- **Not done:**
  - not deployed;
  - there is no authentication. It must run behind a trusted proxy; see [docs/security.md](docs/security.md).

## Results (100k × 128, single machine, 4 vCPU — [docs/benchmarks.md](docs/benchmarks.md))

These results are for the low-intrinsic-dimension `lowrank` dataset, with M = 16 and efConstruction = 200. FAISS `IndexHNSWFlat` was run with the same parameters on the identical files.

| ef | recall@10 | QPS (1 thread) | p50 / p99 µs | FAISS recall@10 | FAISS QPS (1 thread) |
|---|---|---|---|---|---|
| 16 | 0.977 | 9,366 | 97 / 208 | 0.959 | 10,999 |
| 32 | 0.996 | 5,485 | 161 / 319 | 0.993 | 7,886 |
| 64 | 0.9999 | 3,571 | 258 / 522 | 0.9994 | 3,809 |

- **Build:** 50 s, or 2.0k inserts/s through the WAL. FAISS builds the same index in 36 s.
- **Memory:** the graph takes 229 bytes per vector.
- **Snapshot:** 68.7 MB, written in 1.0 s. Recovering from it takes 0.31 s, and search results are identical afterwards.
- **PQ (m = 32):** codes are 15× smaller than the raw vectors. Recall@10 is 0.95 with rerank 4.
- **Filtered search:** recall stays at 1.0 at 50 %, 10 % and 1 % selectivity. Mid and low selectivities are slow, though (≈ 13–18 ms); see the [limitations](docs/limitations.md).

## Quick start

```bash
cargo build --release
./target/release/vectordb serve --data ./data --http 127.0.0.1:6333 --grpc 127.0.0.1:6334 --sync always

curl -X PUT localhost:6333/collections/docs -H 'content-type: application/json' \
     -d '{"dim": 3, "metric": "cosine", "m": 16, "ef_construction": 200, "seed": 42}'
curl -X PUT localhost:6333/collections/docs/points -H 'content-type: application/json' \
     -d '[{"id": 1, "vector": [1, 0, 0], "metadata": {"lang": "en"}},
          {"id": 2, "vector": [0, 1, 0], "metadata": {"lang": "de"}}]'
curl -X POST localhost:6333/collections/docs/search -H 'content-type: application/json' \
     -d '{"vector": [0.9, 0.1, 0], "k": 1, "filter": {"eq": ["lang", "en"]}}'
# {"hits":[{"id":1,"distance":0.0061163306,"metadata":{"lang":"en"}}],"plan":"hnsw-filtered ef=128 (selectivity≈0.500)","micros":15}

./target/release/vectordb check --data ./data     # verify graph/store invariants after recovery
```

## API

| REST | gRPC (`proto/vectordb.proto`) | |
|---|---|---|
| `PUT /collections/:name` | — | Create the collection. Idempotent with the same config; returns 409 if the config differs. |
| `GET` / `DELETE /collections/:name` | `Stats` | Get stats, or drop the collection. |
| `PUT /collections/:name/points` | `Upsert` | Upsert up to 10 000 points. The whole batch is validated before anything is written. |
| `GET` / `DELETE /collections/:name/points/:id` | `Delete` | Get or delete one point. |
| `POST /collections/:name/search` | `Search` | Search. Body: `vector`, `k`, and optionally `ef`, `filter`, `mode` (`hnsw` / `exact` / `pq`) and `rerank`. |
| `POST /collections/:name/snapshot`, `/rebuild`, `/train_pq` | — | Maintenance. |
| `GET /metrics`, `GET /healthz` | — | Prometheus metrics and liveness. |

Filters use the forms `{"eq": [field, value]}`, `{"in": [field, [values]]}`, `{"range": {"field": f, "gte": a, "lte": b}}`, `{"and": [...]}`, `{"or": [...]}` and `{"not": {...}}`.

## Design

See [docs/architecture.md](docs/architecture.md) and the ADRs in [docs/decisions/](docs/decisions/). In short:

- **Write order:** every write is logged (and fsynced with `--sync always`) before it is applied, and it is acknowledged only after that.
- **Recovery:** load the snapshot, then replay the newer WAL records. A torn tail is truncated; corruption anywhere else fails the open.
- **Filters:** they run inside the beam search, so a selective filter never shrinks the result list. Below 2 % estimated selectivity the planner switches to an exact scan.
- **Concurrency:** there is one writer per collection, enforced by a directory lock. Searches run concurrently.

## Documentation

- [Benchmarks](docs/benchmarks.md) (and `benchmarks/` to reproduce them; FAISS baseline in `benchmarks/faiss_baseline.py`)
- [Testing](docs/testing.md)
- [Adversarial audit](docs/audit.md): 6 confirmed defects, each fixed with a regression test, plus 8 suspected issues triaged
- [Security](docs/security.md)
- [Limitations](docs/limitations.md)
- [Requirements matrix](docs/requirements.md)

## Development

```bash
cargo test                                   # all suites
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run --release --bin vdb-bench -- --out /tmp/b --n 5000 --dim 32 --queries 200   # quick benchmark
```

Minimum supported Rust version: 1.89. The `.proto` file is compiled with `protox`, so no `protoc` binary is needed.

## Scorecard (2026-10-16, self-assessed)

| Score | /10 | Why not higher |
|---|---|---|
| Fresher-portfolio strength | 8 | HNSW, PQ, SIMD and WAL from scratch with a FAISS comparison, but only synthetic datasets. |
| Engineering maturity | 8 | Crash-at-every-byte, fault-injection, property and audit tests; no fuzzing and no real-device fsync error. |
| Deployment readiness | 4 | Image builds and runs, but no auth, TLS, replication or backups. |
| Completion of intended scope | 8 | Engine, APIs, durability and Docker done; real datasets and replication are not. |

