//! Dense vector storage: internal ids are consecutive u32 slots; external
//! ids are caller-chosen u64. Deleting marks a tombstone; the slot keeps its
//! vector so graph navigation through it still works until a rebuild
//! compacts the store.

use crate::distance::{dot_scalar, normalize, Metric};
use crate::filter::Metadata;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct VectorStore {
    pub dim: usize,
    pub metric: Metric,
    pub data: Vec<f32>,
    pub ext_ids: Vec<u64>,
    pub deleted: Vec<bool>,
    pub metadata: Vec<Option<Metadata>>,
    pub by_ext: HashMap<u64, u32>, // live external id -> slot
    pub live: usize,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum StoreError {
    #[error("vector has dimension {got}, collection expects {want}")]
    Dimension { got: usize, want: usize },
    #[error("vector contains NaN or infinite values")]
    NonFinite,
    #[error("zero vector cannot be normalised for cosine similarity")]
    ZeroVector,
    #[error("vector magnitude too large: distances would overflow f32")]
    TooLarge,
    #[error("collection is full (2^32 - 1 slots)")]
    Full,
}

impl VectorStore {
    pub fn new(dim: usize, metric: Metric) -> Self {
        VectorStore { dim, metric, data: Vec::new(), ext_ids: Vec::new(), deleted: Vec::new(), metadata: Vec::new(), by_ext: HashMap::new(), live: 0 }
    }

    pub fn len(&self) -> usize {
        self.ext_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ext_ids.is_empty()
    }

    #[inline]
    pub fn vector(&self, slot: u32) -> &[f32] {
        let s = slot as usize * self.dim;
        &self.data[s..s + self.dim]
    }

    /// Validates and (for cosine) normalises a vector before it is stored or
    /// used as a query.
    pub fn prepare(&self, v: &[f32]) -> Result<Vec<f32>, StoreError> {
        if v.len() != self.dim {
            return Err(StoreError::Dimension { got: v.len(), want: self.dim });
        }
        if v.iter().any(|x| !x.is_finite()) {
            return Err(StoreError::NonFinite);
        }
        let mut out = v.to_vec();
        if self.metric == Metric::Cosine {
            if !normalize(&mut out) {
                return Err(StoreError::ZeroVector);
            }
        } else if !(dot_scalar(&out, &out) * 4.0).is_finite() {
            // L2 / inner-product distances between such vectors overflow f32
            return Err(StoreError::TooLarge);
        }
        Ok(out)
    }

    /// Appends a prepared vector; an existing live vector with the same
    /// external id is tombstoned (upsert). Returns (new slot, replaced slot).
    pub fn push(&mut self, ext: u64, v: &[f32], meta: Option<Metadata>) -> Result<(u32, Option<u32>), StoreError> {
        debug_assert_eq!(v.len(), self.dim);
        if self.ext_ids.len() >= u32::MAX as usize {
            return Err(StoreError::Full);
        }
        let replaced = self.delete(ext);
        let slot = self.ext_ids.len() as u32;
        self.data.extend_from_slice(v);
        self.ext_ids.push(ext);
        self.deleted.push(false);
        self.metadata.push(meta);
        self.by_ext.insert(ext, slot);
        self.live += 1;
        Ok((slot, replaced))
    }

    pub fn delete(&mut self, ext: u64) -> Option<u32> {
        let slot = self.by_ext.remove(&ext)?;
        self.deleted[slot as usize] = true;
        self.live -= 1;
        Some(slot)
    }

    #[inline]
    pub fn distance(&self, q: &[f32], slot: u32) -> f32 {
        self.metric.distance(q, self.vector(slot))
    }

    pub fn deleted_fraction(&self) -> f64 {
        if self.is_empty() {
            0.0
        } else {
            1.0 - self.live as f64 / self.len() as f64
        }
    }
}
