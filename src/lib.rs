//! vectordb — a persistent approximate nearest-neighbour search engine.
#![allow(clippy::needless_range_loop)]
pub mod collection;
pub mod distance;
pub mod filter;
pub mod hnsw;
pub mod pq;
pub mod server;
pub mod snapshot;
pub mod store;
pub mod wal;
