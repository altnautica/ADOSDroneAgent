//! The per-plugin MAVLink component-id registrar.

use super::*;

// ---------------------------------------------------------------------
// Component registrar
// ---------------------------------------------------------------------

/// One component-id reservation.
#[derive(Debug, Clone)]
pub(super) struct ComponentRegistration {
    pub(super) plugin_id: String,
    pub(super) component_id: i64,
    pub(super) kind: String,
    /// The connection session that made (or last renewed) the reservation.
    pub(super) session: u64,
}

/// Tracks per-plugin MAVLink component-id reservations.
#[derive(Default)]
pub(super) struct ComponentRegistrar {
    pub(super) by_plugin: BTreeMap<String, BTreeMap<i64, ComponentRegistration>>,
    pub(super) by_component_id: BTreeMap<i64, ComponentRegistration>,
}

impl ComponentRegistrar {
    /// Reserve `comp_id` for `plugin_id` on behalf of `session`. Refuses a
    /// reservation another plugin already holds, with the exact cross-plugin
    /// collision message. The same plugin re-reserving moves the reservation to
    /// the new session.
    pub(super) fn register(
        &mut self,
        plugin_id: &str,
        comp_id: i64,
        kind: &str,
        session: u64,
    ) -> Result<ComponentRegistration, String> {
        if let Some(existing) = self.by_component_id.get(&comp_id) {
            if existing.plugin_id != plugin_id {
                return Err(format!(
                    "component_id {comp_id} already reserved by {}",
                    existing.plugin_id
                ));
            }
        }
        let reg = ComponentRegistration {
            plugin_id: plugin_id.to_string(),
            component_id: comp_id,
            kind: kind.to_string(),
            session,
        };
        self.by_plugin
            .entry(plugin_id.to_string())
            .or_default()
            .insert(comp_id, reg.clone());
        self.by_component_id.insert(comp_id, reg.clone());
        Ok(reg)
    }

    /// The plugin holding the reservation for `comp_id`, if any.
    pub(super) fn holder(&self, comp_id: i64) -> Option<&str> {
        self.by_component_id
            .get(&comp_id)
            .map(|r| r.plugin_id.as_str())
    }

    pub(super) fn is_registered(&self, plugin_id: &str, comp_id: i64) -> bool {
        self.by_plugin
            .get(plugin_id)
            .is_some_and(|m| m.contains_key(&comp_id))
    }

    /// Drop the reservations `session` of `plugin_id` holds. A reservation a
    /// newer session renewed is kept.
    pub(super) fn release_session(&mut self, plugin_id: &str, session: u64) {
        let Some(comps) = self.by_plugin.get_mut(plugin_id) else {
            return;
        };
        let released: Vec<i64> = comps
            .iter()
            .filter(|(_, r)| r.session == session)
            .map(|(id, _)| *id)
            .collect();
        for comp_id in &released {
            comps.remove(comp_id);
            self.by_component_id.remove(comp_id);
        }
        if comps.is_empty() {
            self.by_plugin.remove(plugin_id);
        }
    }
}
