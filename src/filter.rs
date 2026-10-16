//! Metadata and filter expressions.
//!
//! Metadata is a flat map of string keys to scalar values. Filters are a
//! small boolean algebra evaluated per candidate *during* search (never as a
//! post-filter on a fixed top-k), so a selective filter cannot silently
//! shrink the result list.
//!
//! JSON form (used by the REST API):
//! `{"eq": ["lang", "en"]}`, `{"in": ["tag", ["a", "b"]]}`,
//! `{"range": {"field": "year", "gte": 2020, "lte": 2024}}`,
//! `{"and": [...]}`, `{"or": [...]}`, `{"not": {...}}`

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Value {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
    fn loosely_eq(&self, other: &Value) -> bool {
        match (self.as_f64(), other.as_f64()) {
            (Some(a), Some(b)) => a == b,
            _ => self == other,
        }
    }
}

pub type Metadata = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Filter {
    Eq(String, Value),
    In(String, Vec<Value>),
    Range(Range),
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Box<Filter>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub field: String,
    pub gte: Option<f64>,
    pub lte: Option<f64>,
}

pub const MAX_FILTER_DEPTH: usize = 16;
/// Budget for filter nodes plus `in` values together.
pub const MAX_FILTER_NODES: usize = 2048;
pub const MAX_IN_VALUES: usize = 1024;

impl Filter {
    pub fn matches(&self, m: &Metadata) -> bool {
        match self {
            Filter::Eq(k, v) => m.get(k).is_some_and(|x| x.loosely_eq(v)),
            Filter::In(k, vs) => m.get(k).is_some_and(|x| vs.iter().any(|v| x.loosely_eq(v))),
            Filter::Range(r) => m.get(&r.field).and_then(Value::as_f64).is_some_and(|x| r.gte.is_none_or(|g| x >= g) && r.lte.is_none_or(|l| x <= l)),
            Filter::And(fs) => fs.iter().all(|f| f.matches(m)),
            Filter::Or(fs) => fs.iter().any(|f| f.matches(m)),
            Filter::Not(f) => !f.matches(m),
        }
    }

    /// Rejects filters that are too deep or too large to evaluate cheaply
    /// per candidate (a resource-exhaustion guard for the public API).
    pub fn validate(&self) -> Result<(), String> {
        // `nodes` counts filter nodes plus every value of every `in` list, so the
        // per-candidate evaluation cost is bounded as a whole.
        fn walk(f: &Filter, depth: usize, nodes: &mut usize) -> Result<(), String> {
            *nodes += 1;
            if let Filter::In(_, vs) = f {
                if vs.len() > MAX_IN_VALUES {
                    return Err(format!("'in' list longer than {MAX_IN_VALUES}"));
                }
                *nodes += vs.len();
            }
            if depth > MAX_FILTER_DEPTH {
                return Err(format!("filter nesting deeper than {MAX_FILTER_DEPTH}"));
            }
            if *nodes > MAX_FILTER_NODES {
                return Err(format!("filter has more than {MAX_FILTER_NODES} nodes and 'in' values"));
            }
            match f {
                Filter::And(fs) | Filter::Or(fs) => fs.iter().try_for_each(|x| walk(x, depth + 1, nodes)),
                Filter::Not(x) => walk(x, depth + 1, nodes),
                _ => Ok(()),
            }
        }
        walk(self, 0, &mut 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(pairs: &[(&str, Value)]) -> Metadata {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn json_round_trip_and_semantics() {
        let f: Filter = serde_json::from_str(
            r#"{"and": [{"eq": ["lang", "en"]}, {"or": [{"in": ["tag", ["a", "b"]]}, {"range": {"field": "year", "gte": 2020, "lte": null}}]}, {"not": {"eq": ["draft", true]}}]}"#,
        )
        .unwrap();
        let m = md(&[("lang", Value::Str("en".into())), ("year", Value::Int(2021)), ("draft", Value::Bool(false))]);
        assert!(f.matches(&m));
        let m2 = md(&[("lang", Value::Str("en".into())), ("year", Value::Int(2019))]);
        assert!(!f.matches(&m2));
        assert_eq!(serde_json::from_str::<Filter>(&serde_json::to_string(&f).unwrap()).unwrap(), f);
    }

    #[test]
    fn numeric_equality_across_int_and_float() {
        let m = md(&[("x", Value::Float(2.0))]);
        assert!(Filter::Eq("x".into(), Value::Int(2)).matches(&m));
        assert!(!Filter::Eq("missing".into(), Value::Int(2)).matches(&m));
    }

    #[test]
    fn validation_limits() {
        let mut f = Filter::Eq("a".into(), Value::Int(1));
        for _ in 0..20 {
            f = Filter::Not(Box::new(f));
        }
        assert!(f.validate().is_err());
        assert!(Filter::Or(vec![Filter::Eq("a".into(), Value::Int(1)); 3000]).validate().is_err());
        let big_in = Filter::In("a".into(), vec![Value::Int(1); 1000]);
        assert!(big_in.validate().is_ok());
        assert!(Filter::Or(vec![big_in; 3]).validate().is_err()); // 3000 values in total
        assert!(Filter::In("a".into(), vec![Value::Int(1); 1025]).validate().is_err());
    }
}
