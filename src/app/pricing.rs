use serde_json::Value;
use std::collections::HashMap;

pub const DEFAULT_MODELS: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6-sol",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

const TABLE: &[(&str, [f64; 3])] = &[
    ("gpt-6-astra", [10.0, 1.0, 50.0]),
    ("gpt-6-sol", [2.0, 0.2, 10.0]),
    ("gpt-6-luna", [0.1, 0.01, 0.5]),
    ("gpt-5.6-sol", [4.0, 0.4, 20.0]),
    ("gpt-5.6-terra", [2.0, 0.2, 12.0]),
    ("gpt-5.6-luna", [0.2, 0.02, 1.2]),
    ("gpt-5.5-pro", [30.0, 30.0, 180.0]),
    ("gpt-5.5", [5.0, 0.5, 30.0]),
    ("gpt-5.4-pro", [30.0, 30.0, 180.0]),
    ("gpt-5.4-mini", [0.75, 0.075, 4.5]),
    ("gpt-5.4-nano", [0.2, 0.02, 1.25]),
    ("gpt-5.4", [2.5, 0.25, 15.0]),
    ("gpt-5.3", [1.75, 0.175, 14.0]),
    ("gpt-5.2", [1.75, 0.175, 14.0]),
    ("gpt-5.1-codex-mini", [0.25, 0.025, 2.0]),
    ("gpt-5-mini", [0.25, 0.025, 2.0]),
    ("gpt-5.1", [1.25, 0.125, 10.0]),
    ("gpt-5", [1.25, 0.125, 10.0]),
];

pub fn defaults() -> HashMap<String, [f64; 3]> {
    TABLE.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

pub fn rates(model: &str) -> Option<[f64; 3]> {
    let s = crate::app()?.settings();
    if let Some(r) = s.pricing.get(model) {
        return Some(*r);
    }
    s.pricing
        .iter()
        .filter(|(k, _)| model.starts_with(k.as_str()))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, v)| *v)
}

pub fn cost(model: &str, usage: &Value) -> f64 {
    let Some([inp, cached_rate, out]) = rates(model) else { return 0.0 };
    let input = usage["input_tokens"].as_f64().unwrap_or(0.0);
    let cached = usage["input_tokens_details"]["cached_tokens"].as_f64().unwrap_or(0.0);
    let output = usage["output_tokens"].as_f64().unwrap_or(0.0);
    ((input - cached).max(0.0) * inp + cached * cached_rate + output * out) / 1e6
}
