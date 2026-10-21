//! Typed reads of the plugin routes: the install list (`GET /api/plugins`),
//! the bundled catalog (`GET /api/v1/plugins/catalog`), the install preview
//! (`POST /api/plugins/parse_from_url`), and the install request body.

use serde_json::{json, Value};

use crate::overlay::CheckItem;

/// One installed plugin, from `GET /api/plugins` `installs[]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Install {
    pub id: String,
    pub version: String,
    /// `installed` | `enabled` | `running` | `disabled` | `failed` | `incompatible`.
    pub status: String,
    pub signer: Option<String>,
    pub failure_reason: Option<String>,
    pub revocation_pending: bool,
    /// Permission id → granted.
    pub grants: Vec<(String, bool)>,
}

impl Install {
    /// Enabled or running: the plugin is live on this node.
    pub fn enabled(&self) -> bool {
        matches!(self.status.as_str(), "enabled" | "running")
    }

    pub fn granted(&self, perm: &str) -> bool {
        self.grants.iter().any(|(id, g)| id == perm && *g)
    }
}

/// How a status reads: a word class for its colour and glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Live,
    Off,
    Failed,
    Pending,
}

pub fn status_class(status: &str) -> StatusClass {
    match status {
        "enabled" | "running" => StatusClass::Live,
        "disabled" => StatusClass::Off,
        "failed" | "incompatible" => StatusClass::Failed,
        _ => StatusClass::Pending,
    }
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// The install list.
pub fn installs(body: &Value) -> Vec<Install> {
    let Some(list) = body.get("installs").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|i| {
            let id = str_of(i, "plugin_id")?;
            let mut grants: Vec<(String, bool)> = i
                .get("permissions")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, g)| {
                            (
                                k.clone(),
                                g.get("granted").and_then(Value::as_bool).unwrap_or(false),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            grants.sort();
            Some(Install {
                id,
                version: str_of(i, "version").unwrap_or_default(),
                status: str_of(i, "status").unwrap_or_else(|| "installed".into()),
                signer: str_of(i, "signer_id"),
                failure_reason: str_of(i, "failure_reason"),
                revocation_pending: i
                    .get("revocation_pending")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                grants,
            })
        })
        .collect()
}

/// One catalog entry.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub risk: String,
    pub category: String,
    pub description: String,
    pub download_url: String,
    pub sha256: String,
}

/// The catalog's entries.
pub fn catalog(body: &Value) -> Vec<CatalogEntry> {
    let Some(list) = body.get("plugins").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|p| {
            let id = str_of(p, "id")?;
            Some(CatalogEntry {
                name: str_of(p, "name").unwrap_or_else(|| id.clone()),
                id,
                version: str_of(p, "version").unwrap_or_default(),
                risk: str_of(p, "risk").unwrap_or_default(),
                category: str_of(p, "category").unwrap_or_default(),
                description: str_of(p, "description").unwrap_or_default(),
                download_url: str_of(p, "download_url").unwrap_or_default(),
                sha256: str_of(p, "archive_sha256").unwrap_or_default(),
            })
        })
        .collect()
}

/// Catalog entries that are not installed.
pub fn not_installed(catalog: Vec<CatalogEntry>, installs: &[Install]) -> Vec<CatalogEntry> {
    catalog
        .into_iter()
        .filter(|c| !installs.iter().any(|i| i.id == c.id))
        .collect()
}

/// One permission a plugin declares (install preview or detail manifest).
#[derive(Debug, Clone, PartialEq)]
pub struct Perm {
    pub id: String,
    pub required: bool,
    pub label: Option<String>,
    pub description: Option<String>,
    pub risk: Option<String>,
}

impl Perm {
    pub fn title(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }
}

/// `permissions[]` of an install preview or a detail `manifest`.
pub fn perms(body: &Value) -> Vec<Perm> {
    body.get("permissions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|p| {
                    Some(Perm {
                        id: str_of(p, "id")?,
                        required: p.get("required").and_then(Value::as_bool).unwrap_or(true),
                        label: str_of(p, "label"),
                        description: str_of(p, "description"),
                        risk: str_of(p, "risk"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The permission checklist of the install dialog: required ones locked on,
/// optional ones checked by default and toggleable.
pub fn checklist_items(perms: &[Perm]) -> Vec<CheckItem> {
    perms
        .iter()
        .map(|p| {
            let mut label = p.title().to_string();
            if let Some(r) = &p.risk {
                label.push_str(&format!("  [{r}]"));
            }
            if let Some(d) = &p.description {
                label.push_str(&format!(" · {d}"));
            }
            if p.required {
                label.push_str(" (required)");
            }
            CheckItem {
                label,
                checked: true,
                locked: p.required,
                bit: None,
            }
        })
        .collect()
}

/// The permission ids an install requests: every required one plus each
/// checked optional one.
pub fn requested(perms: &[Perm], checked: &[bool]) -> Vec<String> {
    perms
        .iter()
        .enumerate()
        .filter(|(i, p)| p.required || checked.get(*i).copied().unwrap_or(false))
        .map(|(_, p)| p.id.clone())
        .collect()
}

/// The `POST /api/plugins/install_from_url` body.
pub fn install_body(
    url: &str,
    sha256: Option<&str>,
    requested: Vec<String>,
    from_catalog: bool,
) -> Value {
    let mut body = json!({
        "url": url,
        "requested_permissions": requested,
        "from_catalog": from_catalog,
    });
    if let Some(sha) = sha256.filter(|s| !s.is_empty()) {
        body["expected_sha256"] = json!(sha);
    }
    body
}

/// The `POST /api/plugins/parse_from_url` body.
pub fn parse_body(url: &str, sha256: Option<&str>) -> Value {
    let mut body = json!({ "url": url });
    if let Some(sha) = sha256.filter(|s| !s.is_empty()) {
        body["expected_sha256"] = json!(sha);
    }
    body
}

/// Required permissions an install reply did not grant.
pub fn missing_required(perms: &[Perm], reply: &Value) -> Vec<String> {
    let granted: Vec<&str> = reply
        .get("granted")
        .and_then(Value::as_array)
        .map(|g| g.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    perms
        .iter()
        .filter(|p| p.required && !granted.contains(&p.id.as_str()))
        .map(|p| p.id.clone())
        .collect()
}

/// A typed sha256: empty, or 64 hex digits.
pub fn validate_sha(text: &str) -> Result<(), String> {
    let t = text.trim();
    if t.is_empty() || (t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())) {
        Ok(())
    } else {
        Err("Enter 64 hex digits, or leave empty".into())
    }
}

/// A typed archive URL.
pub fn validate_url(text: &str) -> Result<(), String> {
    let t = text.trim();
    if t.starts_with("https://") || t.starts_with("http://") {
        Ok(())
    } else {
        Err("Enter an http(s) URL".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Value {
        json!({"installs": [
            {"plugin_id": "com.example.a", "version": "1.0.0", "status": "running",
             "signer_id": "example", "failure_reason": null, "revocation_pending": false,
             "permissions": {"mavlink.read": {"granted": true, "granted_at": 1}, "vision.read": {"granted": false, "granted_at": null}}},
            {"plugin_id": "com.example.b", "version": "0.2.0", "status": "failed", "failure_reason": "tampered: x",
             "permissions": {}}
        ]})
    }

    #[test]
    fn installs_parse() {
        let i = installs(&list());
        assert_eq!(i.len(), 2);
        assert!(i[0].enabled());
        assert!(i[0].granted("mavlink.read"));
        assert!(!i[0].granted("vision.read"));
        assert_eq!(i[1].failure_reason.as_deref(), Some("tampered: x"));
        assert_eq!(status_class(&i[1].status), StatusClass::Failed);
    }

    #[test]
    fn catalog_minus_installed() {
        let cat = catalog(&json!({"plugins": [
            {"id": "com.example.a", "name": "A", "version": "1.0.0", "risk": "low",
             "download_url": "https://example.com/a.adosplug", "archive_sha256": "aa"},
            {"id": "com.example.c", "name": "C", "version": "2.0.0", "risk": "high",
             "download_url": "https://example.com/c.adosplug", "archive_sha256": "cc"}
        ]}));
        let left = not_installed(cat, &installs(&list()));
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "com.example.c");
        assert_eq!(left[0].sha256, "cc");
    }

    #[test]
    fn install_body_requests_required_and_checked_optional() {
        let p = perms(&json!({"permissions": [
            {"id": "mavlink.read", "required": true, "label": "Read MAVLink"},
            {"id": "vision.read", "required": false},
            {"id": "camera.read", "required": false}
        ]}));
        let items = checklist_items(&p);
        assert!(items[0].locked && items[0].checked);
        assert!(!items[1].locked);
        // The operator unchecks camera.read; a required one stays requested
        // even if the result claims it unchecked.
        let req = requested(&p, &[false, true, false]);
        assert_eq!(
            req,
            vec!["mavlink.read".to_string(), "vision.read".to_string()]
        );
        let body = install_body("https://example.com/c.adosplug", Some("cc"), req, true);
        assert_eq!(
            body,
            json!({
                "url": "https://example.com/c.adosplug",
                "expected_sha256": "cc",
                "requested_permissions": ["mavlink.read", "vision.read"],
                "from_catalog": true
            })
        );
        let no_sha = install_body("https://example.com/x", None, vec![], false);
        assert!(no_sha.get("expected_sha256").is_none());
        assert_eq!(no_sha["from_catalog"], json!(false));
    }

    #[test]
    fn missing_required_grants() {
        let p = perms(&json!({"permissions": [
            {"id": "a", "required": true}, {"id": "b", "required": true}, {"id": "c", "required": false}
        ]}));
        assert_eq!(
            missing_required(&p, &json!({"granted": ["a"]})),
            vec!["b".to_string()]
        );
    }

    #[test]
    fn sha_and_url_validation() {
        assert!(validate_sha("").is_ok());
        assert!(validate_sha(&"a".repeat(64)).is_ok());
        assert!(validate_sha("xyz").is_err());
        assert!(validate_url("https://example.com/p.adosplug").is_ok());
        assert!(validate_url("example.com").is_err());
    }
}
