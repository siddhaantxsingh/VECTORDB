# Testing

Run everything with `cargo test` (the test profile builds at `opt-level = 2`, so the property and crash tests stay fast).

| Suite | Count | What it covers |
|---|---|---|
| unit tests in `src/` | 17 | SIMD kernels vs scalar reference (every length 0..300) and metric semantics; filter JSON round-trip, int/float equality, validation limits; HNSW recall vs exact with invariants, determinism for a seed, allow-predicate never violated with full result lists, empty/single graphs; PQ ADC equals distance to reconstruction, rerank recall and memory, bad `m`; WAL round trip, truncation at every byte, zero-filled tail, mid-log corruption |
| `tests/collection.rs` | 9 | end-to-end on `Collection`: CRUD; input validation and the directory lock; recovery from WAL and from snapshot gives identical results; **crash at every WAL byte offset** recovers exactly the acknowledged prefix; corrupt snapshot fails the open; rebuild/compaction; PQ persistence; a **proptest** of the filtered-search contract (results match exact filtered search, never contain deleted or non-matching ids); concurrent writers and readers |
| `tests/api.rs` | 2 | a real axum + tonic server on ephemeral ports: REST lifecycle and error codes (409 conflicting create, 422 bad name / wrong dimension, 404s, 204 deletes), filters, `/metrics`, snapshot, drop; gRPC round trip, `INVALID_ARGUMENT` for bad filter JSON, a second registry on a live data directory is refused, graceful shutdown and **restart recovery** |
| `tests/audit.rs` | 7 | regression tests for the audit findings (see [audit.md](audit.md)) |
| `tests/audit_pq_rerank.rs` | 1 | unbounded PQ `rerank` (separate binary: before the fix it aborted the process) |

Logs of the runs recorded in this repository are in `test-logs/`.

## Test timing note

`audit.rs::stats_concurrent_with_upserts_does_not_deadlock` used a 30 s bound. Its work takes about 22 s on an idle 4-vCPU machine, with or without the fault hook (A/B in [audit_stats_ab.log](evidence/2026-10-15/audit_stats_ab.log)), so it failed under shared-machine load on 2026-10-15. The bound is now 120 s; a deadlock still never finishes.

## WAL I/O fault injection

* `tests/wal_faults.rs` installs a test-only hook (`Wal::set_fault_hook`, `Collection::set_wal_fault_hook`) that fails one step of `Wal::append`:
  * **write**: half the record is written, then the error is returned;
  * **fsync** (`SyncMode::Always`): the record is complete but `sync_data` fails;
  * **rollback**: the truncate that undoes a failed append also fails.
* For write and fsync errors the test checks that the error reaches the caller, the point is not applied, the LSN is not used, the file is cut back to the last record, later writes succeed, and after a reopen exactly the acknowledged points are present with nothing to repair.
* When the rollback also fails, the log refuses every later write until the process restarts. After a failed fsync the complete but unacknowledged record stays in the file and **does reappear after the restart** (the client got an error, so either outcome is allowed; acknowledged writes are never lost). After a failed short write the fragment is cut off on reopen and the record does not reappear.
* `tests/wal_efbig.rs` lowers `RLIMIT_FSIZE` so a real `write(2)` is cut short by the kernel and then fails with EFBIG (os error 27). It checks the same invariants. It is the only test in its file because the limit is process-wide.
* With the rollback truncate removed, 3 of the 5 tests fail.

## Other checks

* `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean.
* `vectordb check --data DIR` re-runs `Collection::check()` (graph invariants, store/graph/PQ sizes, live counts) on every collection after recovery.
* The benchmark harness asserts graph invariants after the build and that search results are identical before and after recovery.
* CI (`.github/workflows/vectordb.yml`) runs build, tests, fmt, clippy, a small benchmark as a smoke test, and a build on the minimum supported Rust version (1.89).

## Not covered

* No fsync error from a real device (dm-error/dm-flakey need root and a block device). fsync failure is injected with a test hook; the real-kernel test uses EFBIG from `RLIMIT_FSIZE`, which fails the write, not the fsync. ENOSPC is not produced directly.
* No fuzzing of the WAL/snapshot decoders beyond the exhaustive truncation test and the reviewer's targeted cases.
* No multi-hour soak or load test.
