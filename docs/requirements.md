# Requirements matrix — Project 6 (Vector DB)

| Requirement | Status | Evidence |
|---|---|---|
| ANN index (HNSW) from scratch | VERIFIED | `hnsw.rs`; invariant checker; recall vs exact in unit tests and benchmarks; within 0.00–0.04 recall of FAISS IndexHNSWFlat at equal parameters |
| Exact search | VERIFIED | `exact_search`; used as ground truth |
| Distance metrics L2 / IP / cosine | VERIFIED | `distance.rs` tests |
| SIMD distance kernels | VERIFIED | AVX2+FMA vs scalar on every length 0..300 |
| Product quantization | VERIFIED | `pq.rs` tests; benchmark recall/memory (15.4× compression) |
| Metadata filtering | VERIFIED | in-search filtering; proptest contract; benchmark at 50/10/1 % |
| Query planner (filter selectivity) | VERIFIED (fixed threshold) | plan reported in every response; not cost-based |
| Deletes / upserts / compaction | VERIFIED | tombstones; `rebuild`; tests |
| Write-ahead log | VERIFIED | crash at every WAL byte; mid-log corruption (incl. length fields) fails the open |
| Snapshots + recovery | VERIFIED | identical results after recovery (tests + benchmark); corrupt snapshot fails the open |
| fsync on failure paths | VERIFIED (2026-10-16) | `tests/wal_faults.rs`: injected errors at the record write (after a short write), at fsync, and at the rollback itself; acknowledged writes always survive a reopen, a failed write is never applied and its bytes are cut off, the log keeps working after a rolled-back error and refuses writes after a failed rollback until restart. `tests/wal_efbig.rs`: a real kernel error (EFBIG from `RLIMIT_FSIZE`, a genuine short write) with the same checks. Removing the rollback truncate fails 3 of these tests ([wal_fault_mutation.log](evidence/2026-10-15/wal_fault_mutation.log)); runs: [wal_fault_tests.log](evidence/2026-10-15/wal_fault_tests.log). A real fsync EIO from a device was not produced (needs dm-error/root) |
| Concurrency (concurrent reads, safe writes) | VERIFIED | concurrent readers/writers test; lock-order deadlock found and fixed; directory lock |
| REST API | VERIFIED | `tests/api.rs` against a real server |
| gRPC API | VERIFIED | `tests/api.rs` round trip + error codes |
| Metrics | VERIFIED | `/metrics` asserted in API test |
| Graceful shutdown | VERIFIED | test shuts servers down and restarts on the same data; manual SIGTERM run |
| Input validation / DoS limits | VERIFIED | audit regression tests; limits in docs/security.md |
| Benchmarks (recall, QPS, latency percentiles, memory, filtered, PQ, durability) | VERIFIED | `vdb-bench`; `benchmarks/results/2026-10-13/`; docs/benchmarks.md |
| FAISS comparison | VERIFIED | `benchmarks/faiss_baseline.py`, same fvecs and ground truth |
| Real-world datasets (SIFT/GIST/embeddings) | NOT IMPLEMENTED | synthetic datasets only |
| Adversarial audit | VERIFIED | docs/audit.md; fail-before log and commit |
| CI | VERIFIED | `.github/workflows/vectordb.yml` green on GitHub Actions (run 38478453681) |
| Docker image | VERIFIED (2026-10-16) | built with `scripts/docker_build.sh` in 92 s, 156 MB ([docker_build.log](evidence/2026-10-15/docker_build.log)); `scripts/docker_smoke.sh`: runs as uid 10001, create/upsert/filtered search over REST, data survives `docker stop` + a new container on the same volume ([docker_smoke.log](evidence/2026-10-15/docker_smoke.log)). gRPC port not probed. The `debian:bookworm-slim` base came from `mirror.gcr.io` because Docker Hub returned 429 |
| Authentication / TLS | NOT IMPLEMENTED | by design; see docs/security.md |
| Replication / sharding | NOT IMPLEMENTED | out of scope |
