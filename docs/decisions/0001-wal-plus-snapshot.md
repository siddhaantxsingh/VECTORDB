# ADR 0001 — WAL + whole-state snapshots for durability

Status: accepted

Decision: every write is appended to a CRC-framed write-ahead log before it is applied in memory; `snapshot()` serialises the whole in-memory state (vectors, ids, tombstones, metadata, HNSW graph, PQ) to one checksummed file and truncates the WAL.

Why: the index is an in-memory structure; the simplest design whose crash behaviour can be tested exhaustively is "log, then apply" plus an atomic full snapshot. The crash test truncates the WAL at every byte offset and checks recovery.

Consequences: snapshotting clones the whole state in memory and rewrites it in full (O(n) time and 2× peak memory). Recovery from a long WAL re-inserts every vector into the graph, which is slow (≈ the build rate), so operators should snapshot periodically. An incremental/segmented format is future work.
