//! Per-plugin manifest facts the reconciler reads.

use super::*;

impl<H: HostServices> PluginReconciler<H> {
    /// Whether `plugin_id`'s manifest declares a subprocess agent half. A
    /// missing or unparseable manifest is `false`, so that plugin is skipped
    /// rather than the pass failing.
    pub(super) fn is_subprocess_agent(&self, plugin_id: &str) -> bool {
        self.manifest_facts(plugin_id).subprocess
    }

    /// The facts of `plugin_id`'s manifest, parsed once per manifest mtime.
    pub(super) fn manifest_facts(&self, plugin_id: &str) -> ManifestFacts {
        let path = self.install_dir.join(plugin_id).join("manifest.yaml");
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            return ManifestFacts::default();
        };
        let cached = self
            .manifests
            .lock()
            .ok()
            .and_then(|c| c.get(plugin_id).cloned());
        if let Some((seen, facts)) = cached {
            if seen == mtime {
                return facts;
            }
        }
        let facts = read_plugin_manifest(&self.install_dir, plugin_id)
            .map(|m| ManifestFacts {
                subprocess: m.is_subprocess_agent(),
                shared_topics: m.agent.map(|a| a.shared_topics()).unwrap_or_default(),
            })
            .unwrap_or_default();
        if let Ok(mut cache) = self.manifests.lock() {
            cache.insert(plugin_id.to_string(), (mtime, facts.clone()));
        }
        facts
    }

    /// Make the server's shared-topic registry the declarations of the plugins
    /// that should be served. A topic two plugins both declare (install
    /// refuses the second) goes to the first by id, never to both.
    pub(super) fn refresh_shared_topics<'a>(&self, wanted: impl Iterator<Item = &'a String>) {
        let mut topics: Vec<SharedTopic> = Vec::new();
        for id in wanted {
            for decl in self.manifest_facts(id).shared_topics {
                if let Some(owner) = topics.iter().find(|t| t.topic == decl.topic) {
                    tracing::warn!(
                        plugin_id = %id,
                        topic = %decl.topic,
                        owner = %owner.owner,
                        "shared topic already owned; ignoring the second declaration"
                    );
                    continue;
                }
                topics.push(SharedTopic {
                    topic: decl.topic,
                    owner: id.clone(),
                    subscribe_capability: decl.subscribe_capability,
                });
            }
        }
        self.server.shared_topics().replace(topics);
    }
}

/// Read a plugin's manifest off its unpacked install dir. `None` when the
/// manifest is missing or unparseable, in which case the plugin is skipped
/// rather than the whole pass failing.
pub(super) fn read_plugin_manifest(install_dir: &Path, plugin_id: &str) -> Option<PluginManifest> {
    let path = install_dir.join(plugin_id).join("manifest.yaml");
    let text = std::fs::read_to_string(&path).ok()?;
    match PluginManifest::from_yaml_text(&text) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(
                plugin_id,
                path = %path.display(),
                error = %e.0,
                "skipping plugin with an unparseable manifest"
            );
            None
        }
    }
}
