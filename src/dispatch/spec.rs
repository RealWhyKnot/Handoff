use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const DEFAULT_STRING_MAX_BYTES: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    #[default]
    String,
    Int,
    Bool,
    Enum,
    BytesBase64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_empty_vec<T>(v: &[T]) -> bool {
    v.is_empty()
}

fn is_empty_str(s: &str) -> bool {
    s.is_empty()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ParamType,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<i64>,
    #[serde(default, rename = "enum", skip_serializing_if = "is_empty_vec")]
    pub enum_values: Vec<String>,
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_bytes: usize,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub description: String,
}

impl Param {
    pub fn new(name: &str, kind: ParamType) -> Param {
        Param {
            name: name.to_string(),
            kind,
            ..Param::default()
        }
    }

    pub fn required(mut self) -> Param {
        self.required = true;
        self
    }

    pub fn default_value(mut self, value: Value) -> Param {
        self.default = Some(value);
        self
    }

    pub fn range(mut self, min: i64, max: i64) -> Param {
        self.min = Some(min);
        self.max = Some(max);
        self
    }

    pub fn enum_of(mut self, values: &[&str]) -> Param {
        self.enum_values = values.iter().map(|v| v.to_string()).collect();
        self
    }

    pub fn aliases(mut self, aliases: &[&str]) -> Param {
        self.aliases = aliases.iter().map(|v| v.to_string()).collect();
        self
    }

    pub fn max_bytes(mut self, max_bytes: usize) -> Param {
        self.max_bytes = max_bytes;
        self
    }

    pub fn describe(mut self, description: &str) -> Param {
        self.description = description.to_string();
        self
    }

    pub fn effective_max_bytes(&self) -> usize {
        match (self.max_bytes, self.kind) {
            (0, ParamType::String) => DEFAULT_STRING_MAX_BYTES,
            (n, _) => n,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub kind: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub label: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub description: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub risky: bool,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub default_timeout_ms: u32,
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub params: Vec<Param>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clamp {
    pub param: String,
    pub from: i64,
    pub to: i64,
}

pub struct Bound {
    pub args: Map<String, Value>,
    pub clamped: Vec<Clamp>,
    pub ignored: Vec<String>,
}

impl Spec {
    pub fn plain(kind: &str) -> Spec {
        Spec {
            kind: kind.to_string(),
            ..Spec::default()
        }
    }

    pub fn bind(&self, args: &Map<String, Value>) -> Result<Bound, String> {
        let mut out = args.clone();
        let mut clamped = Vec::new();
        let mut known: BTreeSet<&str> = BTreeSet::new();

        for p in &self.params {
            known.insert(&p.name);
            let mut present = out.contains_key(&p.name);
            if !present {
                for alias in &p.aliases {
                    known.insert(alias);
                    if let Some(v) = out.remove(alias) {
                        out.insert(p.name.clone(), v);
                        present = true;
                        break;
                    }
                }
            }

            let blank_enum = p.kind == ParamType::Enum
                && out
                    .get(&p.name)
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.trim().is_empty());
            if present && (blank_enum || out.get(&p.name).is_some_and(Value::is_null)) {
                out.remove(&p.name);
                present = false;
            }

            if !present {
                if p.required {
                    return Err(format!("{}: '{}' is required", self.kind, p.name));
                }
                if let Some(default) = &p.default {
                    out.insert(p.name.clone(), default.clone());
                }
                continue;
            }

            let raw = out[&p.name].clone();
            let (normalized, clamp) = bind_one(&self.kind, p, raw)?;
            out.insert(p.name.clone(), normalized);
            if let Some(c) = clamp {
                clamped.push(c);
            }
        }

        let mut ignored = Vec::new();
        if !self.params.is_empty() {
            for k in out.keys() {
                if !known.contains(k.as_str()) {
                    ignored.push(k.clone());
                }
            }
        }
        Ok(Bound {
            args: out,
            clamped,
            ignored,
        })
    }
}

fn bind_one(kind: &str, p: &Param, raw: Value) -> Result<(Value, Option<Clamp>), String> {
    match p.kind {
        ParamType::Int => {
            let Some(original) = raw.as_i64() else {
                return Err(format!("{kind}: '{}' must be a number", p.name));
            };
            let mut n = original;
            if let Some(min) = p.min {
                if n < min {
                    n = min;
                }
            }
            if let Some(max) = p.max {
                if n > max {
                    n = max;
                }
            }
            let clamp = (n != original).then(|| Clamp {
                param: p.name.clone(),
                from: original,
                to: n,
            });
            Ok((Value::from(n), clamp))
        }
        ParamType::Bool => {
            if raw.is_boolean() {
                Ok((raw, None))
            } else {
                Err(format!("{kind}: '{}' must be true or false", p.name))
            }
        }
        ParamType::Enum => {
            let Some(v) = raw.as_str() else {
                return Err(format!("{kind}: '{}' must be a string", p.name));
            };
            if let Some(allowed) = p
                .enum_values
                .iter()
                .find(|allowed| allowed.eq_ignore_ascii_case(v.trim()))
            {
                Ok((Value::from(allowed.as_str()), None))
            } else {
                Err(format!(
                    "{kind}: '{}' must be one of: {}",
                    p.name,
                    p.enum_values.join(", ")
                ))
            }
        }
        ParamType::String | ParamType::BytesBase64 => {
            let Some(v) = raw.as_str() else {
                return Err(format!("{kind}: '{}' must be a string", p.name));
            };
            let cap = p.effective_max_bytes();
            if cap > 0 && v.len() > cap {
                return Err(format!(
                    "{kind}: '{}' is {} bytes; cap is {}",
                    p.name,
                    v.len(),
                    cap
                ));
            }
            Ok((raw, None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn list_spec() -> Spec {
        Spec {
            kind: "app.list".into(),
            params: vec![
                Param::new("limit", ParamType::Int)
                    .default_value(json!(300))
                    .range(1, 5000)
                    .aliases(&["max_results"]),
                Param::new("match", ParamType::String),
                Param::new("match_mode", ParamType::Enum)
                    .enum_of(&["contains", "prefix", "regex"])
                    .default_value(json!("contains")),
                Param::new("include_hidden", ParamType::Bool).default_value(json!(false)),
            ],
            ..Spec::default()
        }
    }

    #[test]
    fn bind_injects_defaults() {
        let bound = list_spec().bind(&args(json!({}))).unwrap();
        assert!(bound.clamped.is_empty() && bound.ignored.is_empty());
        assert_eq!(bound.args["limit"], json!(300));
        assert_eq!(bound.args["match_mode"], json!("contains"));
        assert_eq!(bound.args["include_hidden"], json!(false));
    }

    #[test]
    fn bind_rejects_wrong_type() {
        let err = list_spec()
            .bind(&args(json!({"limit": "200"})))
            .err()
            .expect("expected a type error for a string limit");
        assert!(err.contains("must be a number"), "{err}");
    }

    #[test]
    fn bind_clamps_and_reports() {
        let bound = list_spec().bind(&args(json!({"limit": 99999}))).unwrap();
        assert_eq!(bound.args["limit"], json!(5000));
        assert_eq!(
            bound.clamped,
            vec![Clamp {
                param: "limit".into(),
                from: 99999,
                to: 5000
            }]
        );
    }

    #[test]
    fn bind_resolves_alias() {
        let bound = list_spec().bind(&args(json!({"max_results": 50}))).unwrap();
        assert_eq!(bound.args["limit"], json!(50));
        assert!(!bound.args.contains_key("max_results"));
        assert!(bound.ignored.is_empty());
    }

    #[test]
    fn bind_prefers_canonical_over_alias() {
        let bound = list_spec()
            .bind(&args(json!({"limit": 10, "max_results": 900})))
            .unwrap();
        assert_eq!(bound.args["limit"], json!(10));
    }

    #[test]
    fn bind_rejects_enum_miss_and_names_legal_values() {
        let err = list_spec()
            .bind(&args(json!({"match_mode": "fuzzy"})))
            .err()
            .expect("expected an enum error");
        for want in ["contains", "prefix", "regex"] {
            assert!(err.contains(want), "error should list {want}: {err}");
        }
    }

    #[test]
    fn bind_matches_enums_case_insensitively_and_returns_the_canonical_value() {
        let bound = list_spec()
            .bind(&args(json!({"match_mode": " Prefix "})))
            .unwrap();
        assert_eq!(bound.args["match_mode"], json!("prefix"));
    }

    #[test]
    fn bind_treats_a_blank_enum_as_absent() {
        let bound = list_spec().bind(&args(json!({"match_mode": ""}))).unwrap();
        assert_eq!(bound.args["match_mode"], json!("contains"));
    }

    #[test]
    fn bind_reports_unknown_keys() {
        let bound = list_spec().bind(&args(json!({"maxresults": 5}))).unwrap();
        assert_eq!(bound.ignored, vec!["maxresults".to_string()]);
    }

    #[test]
    fn bind_required_missing() {
        let spec = Spec {
            kind: "fs.read".into(),
            params: vec![Param::new("path", ParamType::String).required()],
            ..Spec::default()
        };
        let err = spec.bind(&args(json!({}))).err().unwrap();
        assert!(err.contains("'path' is required"), "{err}");
    }

    #[test]
    fn bind_enforces_max_bytes() {
        let spec = Spec {
            kind: "ps.exec".into(),
            params: vec![Param::new("script", ParamType::String)
                .required()
                .max_bytes(8)],
            ..Spec::default()
        };
        let err = spec
            .bind(&args(json!({"script": "way too long"})))
            .err()
            .unwrap();
        assert!(err.contains("cap is 8"), "{err}");
    }

    #[test]
    fn strings_without_a_cap_get_the_default_cap() {
        let spec = list_spec();
        let at_cap = "x".repeat(DEFAULT_STRING_MAX_BYTES);
        let bound = spec.bind(&args(json!({"match": at_cap}))).unwrap();
        assert_eq!(bound.args["match"].as_str().unwrap().len(), 2048);
        let err = spec
            .bind(&args(
                json!({"match": "x".repeat(DEFAULT_STRING_MAX_BYTES + 1)}),
            ))
            .err()
            .unwrap();
        assert_eq!(err, "app.list: 'match' is 2049 bytes; cap is 2048");

        let roomy = Spec {
            kind: "fs.write".into(),
            params: vec![Param::new("content", ParamType::String).max_bytes(1 << 20)],
            ..Spec::default()
        };
        assert!(roomy
            .bind(&args(json!({"content": "x".repeat(100_000)})))
            .is_ok());

        let blob = Spec {
            kind: "fs.upload".into(),
            params: vec![Param::new("data", ParamType::BytesBase64)],
            ..Spec::default()
        };
        assert!(blob
            .bind(&args(json!({"data": "x".repeat(100_000)})))
            .is_ok());
    }

    #[test]
    fn bind_treats_explicit_null_as_absent() {
        let bound = list_spec().bind(&args(json!({"limit": null}))).unwrap();
        assert_eq!(bound.args["limit"], json!(300));
    }

    #[test]
    fn spec_json_field_order_matches_the_wire_contract() {
        let spec = Spec {
            kind: "x.y".into(),
            label: "L".into(),
            description: "D".into(),
            risky: true,
            default_timeout_ms: 5,
            params: vec![Param::new("a", ParamType::Int)
                .required()
                .default_value(json!(1))
                .range(0, 9)
                .aliases(&["b"])
                .describe("d")],
        };
        assert_eq!(
            serde_json::to_string(&spec).unwrap(),
            r#"{"kind":"x.y","label":"L","description":"D","risky":true,"default_timeout_ms":5,"params":[{"name":"a","type":"int","required":true,"default":1,"min":0,"max":9,"aliases":["b"],"description":"d"}]}"#
        );
    }
}
