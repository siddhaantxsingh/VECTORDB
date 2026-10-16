# Limitations

* **Single writer per collection.** Writes are serialised by the WAL mutex, and HNSW insertion is single-threaded, so ingest does not scale with cores (≈ 1–2 k inserts/s at 128 dimensions with M=16, efC=200 on this machine). Searches run concurrently.
* **Tombstones stay in the graph** until `rebuild`, costing memory and some search time; `deleted_fraction` is exported so operators can decide when to rebuild.
* **Snapshots are whole-state.** `snapshot()` clones the entire state in memory (≈ 2× peak memory) and rewrites it in full. Recovery from a long WAL re-inserts every vector into the graph (about the build rate), so snapshot periodically. The server does not snapshot automatically; call `POST /collections/:name/snapshot`.
* **Maintenance blocks traffic.** `snapshot` blocks writes; `train_pq` and `rebuild` also block searches while they run (a 100k × 128 PQ training took tens of seconds).
* **HNSW levels after restart.** The level RNG is reseeded from `seed ^ lsn` on recovery, so after a restart new nodes get different levels than an uninterrupted run would give. Results remain valid; they are not bit-identical to an uninterrupted run.
* **Filter planning is a fixed threshold.** Below 2 % estimated selectivity an exact scan runs; above it, HNSW with `ef / selectivity`. At ≈ 2–10 % selectivity filtered HNSW can be slower than the scan (see benchmarks). Selectivity is estimated from 512 samples, so it is noisy for rare values.
* **Metadata evaluation is slow-ish.** Metadata is a `BTreeMap` per slot; a filter costs a map lookup per candidate (≈ 100–200 ns), which dominates the exact-scan fallback.
* **PQ** uses a single training run with fixed k-means++ seed; no OPQ rotation, no IVF coarse quantiser, so PQ search is a full ADC scan.
* **No authentication or TLS** (see [security.md](security.md)). No replication, sharding or backups beyond copying a snapshot.
* **Benchmarks are synthetic** (two generated datasets) and single-machine; no SIFT/GIST/real embedding datasets were available offline.
* **Advisory directory lock** is not reliable on some network filesystems.
* **I/O fault coverage is partial.** fsync failure is injected with a test hook, and a real kernel write error is produced with `RLIMIT_FSIZE` (EFBIG). A real device fsync error (EIO) and ENOSPC were not produced. After a failed fsync whose rollback also fails, the unacknowledged record can reappear after a restart. Linux may drop dirty pages after a failed fsync; the WAL then refuses writes only if the rollback also fails, and does not re-read the file to check what reached the disk.
* **Docker image:** built and smoke-tested over REST only (the gRPC port was not probed in the container). There is no HEALTHCHECK (no curl in the image). The build needs network access to crates.io; behind a TLS-intercepting proxy pass its CA with `scripts/docker_build.sh`.
