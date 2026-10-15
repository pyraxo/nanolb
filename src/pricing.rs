use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) const DEFAULT_MODELS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6-sol",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

const TABLE: &[(&str, [f64; 3])] = &[
    ("gpt-6.1-sol", [2.0, 0.1, 10.0]),
    ("gpt-6-astra", [10.0, 1.0, 50.0]),
    ("gpt-6-sol", [2.0, 0.2, 10.0]),
    ("gpt-6-luna", [0.1, 0.01, 0.5]),
    ("gpt-5.6-sol", [4.0, 0.4, 20.0]),
    ("gpt-5.6-terra", [2.0, 0.2, 12.0]),
    ("gpt-5.6-luna", [0.2, 0.02, 1.2]),
];

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "[f64; 3]", into = "[f64; 3]")]
pub(crate) struct Rates {
    pub(crate) input: f64,
    pub(crate) cached: f64,
    pub(crate) output: f64,
}

impl From<[f64; 3]> for Rates {
    fn from([input, cached, output]: [f64; 3]) -> Self {
        Self { input, cached, output }
    }
}

impl From<Rates> for [f64; 3] {
    fn from(r: Rates) -> Self {
        [r.input, r.cached, r.output]
    }
}

impl Rates {
    pub(crate) fn cost(self, usage: &Value) -> f64 {
        let input = usage["input_tokens"].as_f64().unwrap_or(0.0);
        let cached = usage["input_tokens_details"]["cached_tokens"].as_f64().unwrap_or(0.0);
        let output = usage["output_tokens"].as_f64().unwrap_or(0.0);
        ((input - cached).max(0.0) * self.input + cached * self.cached + output * self.output) / 1e6
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Pricing(HashMap<String, Rates>);

impl Pricing {
    pub(crate) fn defaults() -> Self {
        Self(TABLE.iter().map(|(k, v)| ((*k).to_owned(), Rates::from(*v))).collect())
    }

    pub(crate) fn rates(&self, model: &str) -> Option<Rates> {
        if let Some(r) = self.0.get(model) {
            return Some(*r);
        }
        self.0
            .iter()
            .filter(|(k, _)| model.starts_with(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, v)| *v)
    }

    pub(crate) fn cost(&self, model: &str, usage: &Value) -> f64 {
        self.rates(model).map_or(0.0, |r| r.cost(usage))
    }
}
