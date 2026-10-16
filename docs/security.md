# Security

## Threat model

The server is meant to run on a private network behind an authenticating proxy. **It has no authentication, authorisation or TLS of its own**: anyone who can reach the ports can read, write and drop every collection. Do not expose ports 6333/6334 publicly.

Within that model, the server must not let a request crash it, escape the data directory, or consume unbounded resources.

## Controls

| Risk | Control |
|---|---|
| Path traversal via collection names | names must match `[a-z0-9_-]{1,64}`; tested with `..%2Fetc` |
| Metrics label injection | same name restriction |
| Oversized requests | REST body ≤ 64 MiB; gRPC messages ≤ 64 MiB each way; ≤ 10 000 points per upsert; metadata ≤ 64 KiB per point |
| Expensive queries | `k ≤ 10 000`, `ef ≤ 10 000`, PQ `rerank ≤ 1000` and `k × rerank ≤ 100 000`; filters: depth ≤ 16, ≤ 1024 values per `in`, ≤ 2048 nodes + values in total; deep JSON nesting is also stopped by serde_json's recursion limit |
| Panics from input | NaN/inf, wrong dimension, zero vectors (cosine), overflow-prone magnitudes, `max_train < 2`, bad PQ `m` are all validated before any work; validation happens before WAL logging |
| Corrupt or malicious files in the data directory | WAL and snapshot decoding bound every length before allocating and verify CRCs; corruption fails the open instead of dropping data silently |
| Two writers on one collection | exclusive lock on `<dir>/LOCK` |
| Internal error details | 500 responses return a generic message; details go to the log |

Process: the Docker image runs as an unprivileged user (uid 10001).

## Known gaps

* No authn/z, TLS, or per-client rate limiting (by design; see threat model).
* `train_pq` and `rebuild` on a large collection block searches for their duration; an unauthenticated client can trigger them (see [limitations.md](limitations.md)).
* No dependency audit (`cargo audit`) has been run.

Report vulnerabilities privately to siddhaantxsingh@gmail.com.
