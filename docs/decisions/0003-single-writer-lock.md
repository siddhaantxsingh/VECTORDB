# ADR 0003 — One writer per collection, enforced by a directory lock

Status: accepted

Decision: writes to a collection are serialised by the WAL mutex; across handles and processes, `Collection::open` takes an exclusive advisory lock (`flock`-style, via `std::fs::File::try_lock`) on `<dir>/LOCK` for the lifetime of the handle.

Why: two handles appending to one WAL would interleave LSNs and corrupt recovery. Before the lock existed, a second `Registry::open` on a live data directory succeeded silently.

Consequences: inserts do not scale with cores (HNSW insertion is the bottleneck); searches do. Advisory locks are not honoured over some network filesystems.
