//! Plugin config writes: declared parameter schemas, validation, the store
//! write and its persistence, and the on-box control surface.

use super::*;

/// One plugin's declared parameter schemas, compiled from one revision of its
/// manifest. A key maps to `None` when its declaration carries no schema or the
/// schema does not compile; a write to it is then allowed.
pub(super) struct ParamSchemas {
    pub(super) manifest: PathBuf,
    pub(super) modified: SystemTime,
    pub(super) by_key: HashMap<String, Option<Arc<jsonschema::JSONSchema>>>,
}

impl ParamSchemas {
    /// Compile every `gcs.contributes.parameters[].schema` in `manifest_text`.
    /// The first declaration of a key decides it.
    pub(super) fn compile(
        plugin_id: &str,
        manifest: PathBuf,
        modified: SystemTime,
        manifest_text: &str,
    ) -> Self {
        let mut by_key = HashMap::new();
        for (key, schema) in declared_parameter_schemas(manifest_text) {
            if by_key.contains_key(&key) {
                continue;
            }
            let compiled = schema.and_then(|s| match jsonschema::JSONSchema::compile(&s) {
                Ok(c) => Some(Arc::new(c)),
                Err(e) => {
                    tracing::debug!(plugin_id, key = %key, error = %e, "parameter schema did not compile; skipping value validation");
                    None
                }
            });
            by_key.insert(key, compiled);
        }
        ParamSchemas {
            manifest,
            modified,
            by_key,
        }
    }
}

/// Every `(key, schema)` a manifest declares under
/// `gcs.contributes.parameters`, each schema in its Draft-07 validation form. An
/// unparseable manifest declares nothing.
pub(super) fn declared_parameter_schemas(
    manifest_text: &str,
) -> Vec<(String, Option<serde_json::Value>)> {
    let Ok(manifest) = crate::manifest::PluginManifest::from_yaml_text(manifest_text) else {
        return Vec::new();
    };
    let Some(params) = manifest
        .gcs
        .as_ref()
        .and_then(|g| g.extra.get("contributes"))
        .and_then(|c| c.get("parameters"))
        .and_then(|p| p.as_sequence())
    else {
        return Vec::new();
    };
    params
        .iter()
        .filter_map(|p| {
            let key = p.get("key")?.as_str()?.to_string();
            let schema = p
                .get("schema")
                .and_then(|s| serde_json::to_value(s).ok())
                .map(to_param_json_schema);
            Some((key, schema))
        })
        .collect()
}

/// The Draft-07 validation schema for a plugin parameter — mirrors the GCS
/// `toJsonSchema`: an `enum` reduces to membership-only (`{ enum }`) and the
/// UI-only `step` is dropped, so the agent + GCS accept/reject identically.
pub(super) fn to_param_json_schema(raw: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = raw else {
        return raw;
    };
    if let Some(en) = obj.get("enum") {
        return serde_json::json!({ "enum": en.clone() });
    }
    obj.remove("step");
    serde_json::Value::Object(obj)
}

impl RealHost {
    /// The compiled schema for `(plugin_id, key)`, or `None` when the write is
    /// unconstrained: no lookup, no manifest, no schema for the key, or a schema
    /// that does not compile. The manifest is found through the runtime lookup
    /// and compiled once per revision (keyed by its mtime). The lookup, the stat
    /// and a cache-miss read all run on the blocking pool.
    pub(super) async fn parameter_schema(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> Option<Arc<jsonschema::JSONSchema>> {
        let lookup = Arc::clone(self.plugin_runtime_lookup.as_ref()?);
        let cache = Arc::clone(&self.param_schemas);
        let plugin_id = plugin_id.to_string();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            let (install_dir, _) = lookup(&plugin_id)?;
            let manifest = install_dir.join("manifest.yaml");
            let modified = std::fs::metadata(&manifest)
                .and_then(|m| m.modified())
                .ok()?;
            let cached = cache
                .lock()
                .expect("parameter schema cache poisoned")
                .get(&plugin_id)
                .filter(|c| c.manifest == manifest && c.modified == modified)
                .cloned();
            let schemas = match cached {
                Some(schemas) => schemas,
                None => {
                    let text = std::fs::read_to_string(&manifest).ok()?;
                    let compiled =
                        Arc::new(ParamSchemas::compile(&plugin_id, manifest, modified, &text));
                    cache
                        .lock()
                        .expect("parameter schema cache poisoned")
                        .insert(plugin_id, Arc::clone(&compiled));
                    compiled
                }
            };
            schemas.by_key.get(&key).cloned().flatten()
        })
        .await
        .ok()
        .flatten()
    }

    /// Validate a config value against the plugin's declared parameter schema
    /// before it is persisted — the agent half of the shared validator, so the
    /// agent never trusts the GCS form. A missing schema, a
    /// non-JSON-representable value, or a schema that does not compile all ALLOW
    /// the write (graceful degradation); only a value a valid schema rejects is
    /// refused.
    pub(super) async fn validate_plugin_config_value(
        &self,
        plugin_id: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), String> {
        let Some(schema) = self.parameter_schema(plugin_id, key).await else {
            return Ok(());
        };
        let Ok(instance) = serde_json::to_value(value) else {
            return Ok(());
        };
        if let Err(errors) = schema.validate(&instance) {
            let first = errors
                .map(|e| e.to_string())
                .next()
                .unwrap_or_else(|| "does not match the parameter schema".to_string());
            return Err(format!(
                "value for '{key}' violates the parameter schema: {first}"
            ));
        }
        Ok(())
    }

    /// Validate, store and persist one config write; shared by a plugin's own
    /// `config.set` and the control socket. Returns the effective scope (`drone`
    /// collapses to `global` when no device id is bound, matching
    /// `ConfigStore::set`).
    pub(super) async fn store_config(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<String, String> {
        self.validate_plugin_config_value(plugin_id, key, &value)
            .await?;
        let agent_id = self.agent_id_for(plugin_id);
        let snapshot = self
            .config
            .lock()
            .expect("config mutex poisoned")
            .set(plugin_id, key, value, scope, &agent_id)?;
        if let Some(snapshot) = snapshot {
            self.persist_config(snapshot).await;
        }
        let effective = if scope == "drone" && agent_id.is_empty() {
            "global"
        } else {
            scope
        };
        Ok(effective.to_string())
    }

    /// Write a config snapshot on the blocking pool. Writers are serialized and a
    /// snapshot older than the one already on disk is dropped, so two concurrent
    /// sets can never leave the older store on disk. A failure is logged and
    /// swallowed: durability is best-effort and never fails a plugin's
    /// config.set.
    pub(super) async fn persist_config(&self, snapshot: ConfigSnapshot) {
        let written = Arc::clone(&self.config_written);
        let path = snapshot.path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut last = written.lock().expect("config persist mutex poisoned");
            if snapshot.generation <= *last {
                return Ok(());
            }
            write_json_owner_only(&snapshot.path, &snapshot.json)?;
            *last = snapshot.generation;
            Ok::<(), std::io::Error>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(path = %path.display(), error = %e, "plugin config persist failed")
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "plugin config persist task failed")
            }
        }
    }

    /// Apply a config write that originates off the per-plugin RPC path — the
    /// on-box control socket (a GCS skill toggle / per-drone settings change for
    /// a plugin the writer is not). It resolves the per-drone scope via the same
    /// `agent_id_for` lookup `config.set` uses, so an operator's `active` flip
    /// lands in the exact per-drone namespace the plugin reads, and the store
    /// flushes its 0600 JSON on the set. The trust boundary is the control socket
    /// itself (on-box, owner+group); there is no capability token here. Returns
    /// the effective scope.
    pub async fn apply_config_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<String, String> {
        if plugin_id.is_empty() {
            return Err("plugin_id must be a non-empty string".to_string());
        }
        if key.is_empty() {
            return Err("key must be a non-empty string".to_string());
        }
        if scope != "drone" && scope != "global" {
            return Err(format!(
                "scope must be drone or global, got {}",
                py_repr(scope)
            ));
        }
        self.store_config(plugin_id, key, value, scope).await
    }
}

impl crate::control::ConfigControl for RealHost {
    async fn apply_config_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: Value,
        scope: &str,
    ) -> Result<String, String> {
        RealHost::apply_config_set(self, plugin_id, key, value, scope).await
    }

    fn config_snapshot(&self, plugin_id: &str) -> Result<Value, String> {
        if plugin_id.is_empty() {
            return Err("plugin_id must be a non-empty string".to_string());
        }
        let agent_id = self.agent_id_for(plugin_id);
        let values = self
            .config
            .lock()
            .expect("config mutex poisoned")
            .effective(plugin_id, &agent_id);
        Ok(Value::Map(
            values
                .into_iter()
                .map(|(k, v)| (Value::from(k), v))
                .collect(),
        ))
    }
}
