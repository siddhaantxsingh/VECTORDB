# ADR 0002 — Filters inside the beam search, exact fallback below 2 % selectivity

Status: accepted

Decision: metadata filters are evaluated per candidate inside HNSW's beam search (non-matching nodes are traversed but never returned). The planner estimates selectivity from ≤ 512 sampled slots; below 2 % it runs an exact scan over matching slots, otherwise HNSW with `ef / selectivity`.

Why: post-filtering a fixed top-k silently returns fewer than k results for selective filters. In-search filtering keeps result counts correct, and the exact fallback avoids the regime where the beam must traverse most of the graph to find k matches.

Consequences: at mid selectivities (≈ 2–10 %) filtered HNSW can be slower than a brute-force scan (see docs/benchmarks.md); the 2 % threshold is a fixed constant, not cost-based. A cost-based planner is future work.
