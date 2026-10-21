//! Editable rows for agent config keys: the kind, default and secret flag from
//! the config schema (`GET /api/config/schema`), the value from the effective
//! config (`GET /api/config`), and the label/help text. Shared by the Config
//! pages, the All settings browser and bespoke pages that show config keys.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use serde_json::Value;

use super::registry;
use super::row::EditRow;
use super::rows_view::{note, Item};
use super::schema::{flatten, get, title_case, Kind, Leaf};
use crate::app::Core;
use crate::poll::{ResourceId, Sub};

/// The effective config is re-read this often while a config page shows.
pub const CONFIG_EVERY_MS: u64 = 15_000;
/// A failed schema read retries this often until it lands.
pub const SCHEMA_RETRY_MS: u64 = 10_000;

/// Resources a page showing config rows needs: the effective config, and the
/// schema until it has loaded once this session.
pub fn subs(core: &Core) -> Vec<Sub> {
    let mut out = vec![Sub::every_ms(ResourceId::Config, "/api/config", CONFIG_EVERY_MS)];
    if core.value(&ResourceId::ConfigSchema).is_none() {
        out.push(Sub::every_ms(ResourceId::ConfigSchema, "/api/config/schema", SCHEMA_RETRY_MS));
    }
    out
}

type Cached = (Instant, usize, Rc<Vec<Leaf>>);

thread_local! {
    static LEAVES: RefCell<Option<Cached>> = const { RefCell::new(None) };
}

/// The flattened schema leaves, once the schema has loaded. Flattened once per
/// schema read and shared.
pub fn leaves(core: &Core) -> Option<Rc<Vec<Leaf>>> {
    let slot = core.slot(&ResourceId::ConfigSchema)?;
    let schema = slot.value.as_ref()?;
    let addr = schema as *const Value as usize;
    LEAVES.with(|cell| {
        let mut cache = cell.borrow_mut();
        if let Some((at, ptr, leaves)) = cache.as_ref() {
            if *at == slot.at && *ptr == addr {
                return Some(Rc::clone(leaves));
            }
        }
        let leaves = Rc::new(flatten(schema));
        *cache = Some((slot.at, addr, Rc::clone(&leaves)));
        Some(leaves)
    })
}

/// The row label for a leaf: the curated label, else the schema title, else
/// the key's last segment title-cased.
pub fn label_of(key: &str, leaf: Option<&Leaf>) -> String {
    if let Some(r) = registry::row_for(key).filter(|r| !r.label.is_empty()) {
        return r.label.to_string();
    }
    match leaf {
        Some(l) => l.title.clone().unwrap_or_else(|| l.fallback_label()),
        None => title_case(key.rsplit('.').next().unwrap_or(key)),
    }
}

/// The row help for a leaf: the curated help, else the schema description.
pub fn help_of(key: &str, leaf: Option<&Leaf>) -> String {
    if let Some(r) = registry::row_for(key).filter(|r| !r.help.is_empty()) {
        return r.help.to_string();
    }
    leaf.and_then(|l| l.description.clone()).unwrap_or_default()
}

/// The kind of a key the schema does not describe, guessed from its value.
fn guessed_kind(value: Option<&Value>) -> Kind {
    match value {
        Some(Value::Bool(_)) => Kind::Bool,
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Kind::Int { min: None, max: None },
        Some(Value::Number(_)) => Kind::Num { min: None, max: None },
        Some(Value::String(_)) => Kind::Str { pattern: None },
        _ => Kind::Opaque,
    }
}

/// Build the row for a leaf with an explicit label and help.
pub fn leaf_row(core: &Core, leaf: &Leaf, label: String, help: String) -> EditRow {
    let value = core
        .value(&ResourceId::Config)
        .and_then(|c| get(c, &leaf.key))
        .cloned();
    EditRow {
        key: leaf.key.clone(),
        label,
        help,
        read_only: leaf.kind == Kind::Opaque,
        kind: leaf.kind.clone(),
        value,
        default: leaf.default.clone(),
        secret: leaf.secret,
        nullable: leaf.nullable,
    }
}

/// The editable row for config `key`. An empty `label`/`help` falls back to
/// the curated text, then the schema. A key the schema does not describe (or
/// before the schema loads) is read-only.
pub fn config_row(core: &Core, key: &str, label: &str, help: &str) -> EditRow {
    let all = leaves(core);
    let leaf = all.as_ref().and_then(|l| l.iter().find(|x| x.key == key));
    let label = if label.is_empty() {
        label_of(key, leaf)
    } else {
        label.to_string()
    };
    let help = if help.is_empty() {
        help_of(key, leaf)
    } else {
        help.to_string()
    };
    match leaf {
        Some(leaf) => leaf_row(core, leaf, label, help),
        None => {
            let value = core.value(&ResourceId::Config).and_then(|c| get(c, key)).cloned();
            EditRow {
                key: key.to_string(),
                label,
                help,
                kind: guessed_kind(value.as_ref()),
                value,
                default: None,
                secret: false,
                nullable: false,
                read_only: true,
            }
        }
    }
}

/// A status line while the config or its schema has not loaded (or failed).
pub fn status_note(core: &Core) -> Option<Item> {
    for (id, what) in [
        (ResourceId::Config, "the config"),
        (ResourceId::ConfigSchema, "the config schema"),
    ] {
        let slot = core.slot(&id);
        if slot.and_then(|s| s.value.as_ref()).is_some() {
            continue;
        }
        return Some(match slot.and_then(|s| s.error.as_deref()) {
            Some(err) => note(core, format!("Could not read {what}: {err}"), true),
            None => note(core, format!("Reading {what}…"), false),
        });
    }
    None
}
