//! The agent config schema (`GET /api/config/schema`, a JSON Schema document)
//! flattened to one [`Leaf`] per scalar key, plus the value kinds every
//! editable row is built from.

use serde_json::{Map, Value};

/// How a value is edited.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Bool,
    Int { min: Option<i64>, max: Option<i64> },
    Num { min: Option<f64>, max: Option<f64> },
    Str { pattern: Option<String> },
    Enum(Vec<Value>),
    /// Arrays and objects with no scalar leaves: shown, not edited.
    Opaque,
}

/// One dotted config key.
#[derive(Debug, Clone, PartialEq)]
pub struct Leaf {
    /// Dotted key, e.g. `mavlink.baud_rate`.
    pub key: String,
    pub kind: Kind,
    pub default: Option<Value>,
    /// Schema `description`, else `None`.
    pub description: Option<String>,
    /// Schema `title`, else `None`.
    pub title: Option<String>,
    /// `x-secret: true`: masked, never echoed.
    pub secret: bool,
    /// An `anyOf`/`oneOf` with `null`: the value can be cleared to null.
    pub nullable: bool,
}

impl Leaf {
    /// The top-level section (`mavlink` for `mavlink.baud_rate`).
    pub fn section(&self) -> &str {
        self.key.split('.').next().unwrap_or("")
    }

    /// The row label when no curated label exists: the description's first
    /// sentence is too long, so the key's last segment, title-cased.
    pub fn fallback_label(&self) -> String {
        title_case(self.key.rsplit('.').next().unwrap_or(&self.key))
    }
}

/// `baud_rate` → `Baud rate`.
pub fn title_case(segment: &str) -> String {
    let spaced = segment.replace(['_', '-'], " ");
    let mut c = spaced.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Resolve a local `$ref` (`#/$defs/Name`).
fn deref<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    let mut cur = node;
    for _ in 0..16 {
        match cur.get("$ref").and_then(Value::as_str) {
            Some(r) => {
                let Some(path) = r.strip_prefix("#/") else { return cur };
                let target = path.split('/').try_fold(root, |acc, k| acc.get(k));
                match target {
                    Some(t) => cur = t,
                    None => return cur,
                }
            }
            None => return cur,
        }
    }
    cur
}

/// Split a nullable union into its non-null branch.
fn non_null<'a>(root: &'a Value, node: &'a Value) -> (&'a Value, bool) {
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = node.get(key).and_then(Value::as_array) {
            let is_null = |b: &Value| b.get("type").and_then(Value::as_str) == Some("null");
            let nullable = branches.iter().any(is_null);
            if let Some(other) = branches.iter().find(|b| !is_null(b)) {
                return (deref(root, other), nullable);
            }
        }
    }
    (deref(root, node), false)
}

fn kind_of(node: &Value) -> Kind {
    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        return Kind::Enum(values.clone());
    }
    match node.get("type").and_then(Value::as_str) {
        Some("boolean") => Kind::Bool,
        Some("integer") => Kind::Int {
            min: node.get("minimum").and_then(Value::as_i64),
            max: node.get("maximum").and_then(Value::as_i64),
        },
        Some("number") => Kind::Num {
            min: node.get("minimum").and_then(Value::as_f64),
            max: node.get("maximum").and_then(Value::as_f64),
        },
        Some("string") => Kind::Str {
            pattern: node.get("pattern").and_then(Value::as_str).map(str::to_string),
        },
        _ => Kind::Opaque,
    }
}

fn walk(root: &Value, props: &Map<String, Value>, prefix: &str, out: &mut Vec<Leaf>, depth: usize) {
    for (name, raw) in props {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let (node, nullable) = non_null(root, raw);
        // A nested object recurses into its scalar leaves.
        if let (Some(sub), true) = (node.get("properties").and_then(Value::as_object), depth < 8) {
            let before = out.len();
            walk(root, sub, &key, out, depth + 1);
            if out.len() > before {
                continue;
            }
        }
        let text = |k: &str| {
            raw.get(k)
                .or_else(|| node.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        out.push(Leaf {
            kind: kind_of(node),
            default: raw.get("default").or_else(|| node.get("default")).cloned(),
            description: text("description"),
            title: text("title"),
            secret: raw.get("x-secret").or_else(|| node.get("x-secret")).and_then(Value::as_bool)
                == Some(true),
            nullable,
            key,
        });
    }
}

/// Flatten a config schema into its leaves, in schema order.
pub fn flatten(schema: &Value) -> Vec<Leaf> {
    let mut out = Vec::new();
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        walk(schema, props, "", &mut out, 0);
    }
    out
}

/// Look up a dotted key in a config document.
pub fn get<'a>(config: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(config, |acc, k| acc.get(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "type": "object",
            "properties": {
                "mavlink": {"$ref": "#/$defs/Mavlink"},
                "battery": {"$ref": "#/$defs/Battery"},
                "api": {"$ref": "#/$defs/Api"}
            },
            "$defs": {
                "Mavlink": {"properties": {
                    "source": {"enum": ["auto", "serial"], "default": "auto", "type": "string"},
                    "baud_rate": {"type": "integer", "default": 57600},
                    "endpoints": {"type": "array", "items": {"type": "object"}}
                }},
                "Battery": {"properties": {
                    "low_cell_mv": {"type": "integer", "minimum": 2500, "maximum": 4200, "default": 3500},
                    "gauge": {"anyOf": [{"$ref": "#/$defs/Gauge"}, {"type": "null"}], "default": null},
                    "capacity_mah": {"anyOf": [{"type": "integer", "minimum": 1}, {"type": "null"}], "description": "Pack size"}
                }},
                "Gauge": {"properties": {"enabled": {"type": "boolean", "default": false}}},
                "Api": {"properties": {
                    "api_key": {"type": "string", "x-secret": true, "default": ""}
                }}
            }
        })
    }

    #[test]
    fn leaves_carry_kinds_bounds_and_secrets() {
        let leaves = flatten(&fixture());
        let by = |k: &str| leaves.iter().find(|l| l.key == k).unwrap_or_else(|| panic!("{k} missing: {leaves:?}"));
        assert_eq!(by("mavlink.source").kind, Kind::Enum(vec![json!("auto"), json!("serial")]));
        assert_eq!(by("mavlink.baud_rate").kind, Kind::Int { min: None, max: None });
        assert_eq!(by("mavlink.endpoints").kind, Kind::Opaque);
        assert_eq!(
            by("battery.low_cell_mv").kind,
            Kind::Int { min: Some(2500), max: Some(4200) }
        );
        // A nullable object recurses into its leaves.
        assert_eq!(by("battery.gauge.enabled").kind, Kind::Bool);
        // A nullable scalar keeps its bounds and is marked clearable.
        let cap = by("battery.capacity_mah");
        assert!(cap.nullable);
        assert_eq!(cap.kind, Kind::Int { min: Some(1), max: None });
        assert_eq!(cap.description.as_deref(), Some("Pack size"));
        assert!(by("api.api_key").secret);
        assert!(!by("mavlink.source").secret);
        assert_eq!(by("mavlink.baud_rate").section(), "mavlink");
        assert_eq!(by("battery.low_cell_mv").fallback_label(), "Low cell mv");
    }

    #[test]
    fn dotted_get_walks_the_document() {
        let cfg = json!({"mavlink": {"baud_rate": 921600}});
        assert_eq!(get(&cfg, "mavlink.baud_rate"), Some(&json!(921600)));
        assert_eq!(get(&cfg, "mavlink.nope"), None);
    }
}
