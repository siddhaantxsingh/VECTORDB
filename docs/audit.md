# Adversarial audit

An independent reviewer audited the code, working on a scratch copy, after the server was first made to work. It was asked to find correctness, durability, concurrency and security defects reachable through the public API, and to prove each one with a failing test.

**Evidence.** The reproductions were committed **before** any fix (commit "adversarial audit reproductions (failing, before fixes)"), so fail-before can be re-run by checking out that commit. Logs:

- `test-logs/audit-fail-before.txt`: the 5 tests in `tests/audit.rs` fail, and `tests/audit_pq_rerank.rs` aborts the process (`memory allocation of 87960930222088 bytes failed`, SIGABRT).
- `test-logs/cargo-test-after-audit.txt`: all suites pass after the fixes.

## Confirmed defects (all fixed)

| # | Severity | Defect | Fix | Regression test |
|---|---|---|---|---|
| 1 | high | **Deadlock.** `stats()` took the index read lock and then the WAL mutex; writes take them in the opposite order. A `GET /collections/x`, `/metrics` or gRPC `Stats` during an upsert hung both threads. `/metrics` holds the registry lock, so create and drop then hung too. | `stats()` reads the WAL size first and releases it. The lock order is now WAL → index everywhere. | `stats_concurrent_with_upserts_does_not_deadlock` |
| 2 | high | **One request aborts the server.** PQ search allocated `k × rerank` heap slots with no bound on `rerank`. | `rerank` must be 1..1000 and `k × rerank` ≤ 100 000, otherwise 422 / INVALID_ARGUMENT. | `pq_search_huge_rerank_is_rejected` (own file, because before the fix it kills the test process) |
| 3 | medium-high | **Drop/create race.** `drop_collection` unregistered the collection and then deleted its directory outside the registry lock. A concurrent `PUT` of the same name re-opened the old directory, which was then deleted underneath it: writes were acknowledged and then lost. | The directory is renamed to `.dropped-<name>-<ns>` while the lock is held and deleted afterwards. `Registry::open` removes leftovers from a crash mid-drop. | `drop_racing_create_does_not_lose_a_live_collection` |
| 4 | medium | **Silent WAL truncation.** A corrupted length field in the middle of the log was treated as a torn tail, so every later acknowledged record was silently discarded. | A length that runs past the end of the file only counts as a torn tail if no complete, checksummed record follows it. Otherwise the open fails with `Corrupt`. | `mid_log_length_corruption_is_not_silent_loss` |
| 5 | low-medium | `POST /train_pq` with `max_train: 0` panicked inside k-means and returned 500. | `max_train ≥ 2` is validated (422). | `train_pq_max_train_zero_is_an_error_not_a_panic` |
| 6 | low | Cosine collections rejected large finite vectors (`[1e20, 1e20]`) as "zero vector", because the squared norm overflowed. | `normalize` scales by the largest magnitude first. | `cosine_accepts_large_finite_vectors` |

## Suspected issues

The reviewer reported these as unconfirmed. Status after this pass:

| # | Issue | Status |
|---|---|---|
| S1 | A failed `fsync` left the record in the file. The client saw an error, but the write was replayed after a restart. | **Fixed.** On any append failure the file is cut back to the last complete record. If the rollback itself fails, the log refuses further writes. No fault-injection test (needs an I/O shim). |
| S2 | A short write (ENOSPC) left a fragment that later appends landed behind. | **Fixed** by the same rollback. Untested, as for S1. |
| S3 | `snapshot`, `train_pq` and `rebuild` hold the WAL mutex for the whole operation; `train_pq` and `rebuild` also hold the index write lock, so they block searches. | **Open; documented** in [limitations.md](limitations.md). The deadlock part (#1) is fixed. |
| S4 | The data directory was not fsynced after a collection directory was created. | **Fixed:** the parent directory is fsynced on create. |
| S5 | A REST batch upsert with one invalid point left the earlier points applied. | **Fixed:** `Collection::upsert_batch` validates every point before writing any, and names the bad point in the error. Test: `batch_upsert_is_all_or_nothing_on_validation_errors`. An I/O error part-way through can still leave a durable prefix. |
| S6 | Filter cost was bounded only per node: 256 nodes × 1024 `in` values gave about 262k comparisons per candidate. | **Fixed:** one budget of 2048 for nodes plus `in` values together; at most 1024 values per list. Unit test in `filter.rs`. |
| S7 | gRPC had no response-size limit. `stats()` runs on async workers. | **Fixed (size):** gRPC decode and encode are capped at 64 MiB, and gRPC upserts at 10 000 points like REST. `stats()` is now cheap and non-blocking after #1, so it was left on the async workers. |
| S8 | Very large finite vectors give inf/NaN distances under L2 and inner product. | **Fixed:** `prepare` rejects vectors whose squared norm × 4 overflows f32 (`TooLarge`, 422). Test: `huge_vectors_are_rejected_for_l2`. |

## Checked and found sound

The reviewer also checked these and found no defect:

- **Snapshot decoding:** every length is bounded before allocation, and the CRC is checked.
- **WAL decoding:** all reads are bounds-checked.
- **Crash between snapshot and WAL reset:** records already in the snapshot are skipped on replay.
- **Upserts and deletes:** upserting an existing id tombstones the old slot and keeps PQ codes aligned; delete followed by re-insert works.
- **Rebuild:** keeps metadata and PQ codebooks.
- **PQ training edge cases:** 0 or 1 vectors, and `m` not dividing the dimension, both return errors.
- **Query parameters:** `k` larger than the live count works; NaN, infinite and zero query vectors are rejected.
- **Names and paths:** metric label injection and path traversal are impossible, because names are restricted to `[a-z0-9_-]`.
- **Limits and concurrency:** the u32 slot limit is guarded; dropping a collection during in-flight searches is safe.
