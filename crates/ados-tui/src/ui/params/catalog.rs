//! Params tab metadata: pick the catalog this FC needs and decode it off the
//! render thread, once per catalog per session.

use std::sync::mpsc::TryRecvError;
use std::sync::Arc;

use serde_json::Value;

use super::ParamsTab;
use crate::app::{at, Core};
use crate::params::metadata;
use crate::poll::ResourceId;
use crate::toast::ToastKind;

/// The catalog this FC needs. ArduPilot waits for the snapshot's vehicle so a
/// plane does not briefly load the copter catalog.
fn wanted_key(core: &Core) -> Option<&'static str> {
    let firmware = core.dash()?.fc_firmware.clone()?;
    if firmware == "ardupilot" && core.slot(&ResourceId::Snapshot).is_none() {
        return None;
    }
    let vehicle = core
        .value(&ResourceId::Snapshot)
        .and_then(|v| at(v, "fc.vehicle"))
        .and_then(Value::as_str);
    metadata::catalog_key(&firmware, vehicle)
}

impl ParamsTab {
    /// Load (or switch to) the catalog this FC needs, and its favourites.
    pub(super) fn ensure_catalog(&mut self, core: &Core) {
        let Some(key) = wanted_key(core) else { return };
        if self.fav_key != Some(key) {
            self.fav_key = Some(key);
            let favs = core
                .store
                .prefs
                .favorite_params
                .get(key)
                .cloned()
                .unwrap_or_default();
            self.store.set_favourites(favs);
        }
        if self.store.catalog().map(|c| c.key) == Some(key) {
            return;
        }
        if let Some(cat) = self.catalogs.get(key) {
            self.store.set_catalog(Some(Arc::clone(cat)));
            self.clamp();
            return;
        }
        let busy = self.loading.as_ref().map(|(k, _)| *k);
        if busy != Some(key) && !self.failed.contains(&key) {
            self.loading = Some((key, metadata::spawn_decode(key)));
        }
    }

    /// Pick up a finished background decode.
    pub(super) fn drain_catalog(&mut self, core: &mut Core) {
        let Some((key, rx)) = &self.loading else {
            return;
        };
        let key = *key;
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.loading = None,
            Ok(result) => {
                self.loading = None;
                core.dirty = true;
                match result {
                    Ok(cat) => {
                        self.catalogs.insert(key, Arc::new(cat));
                        self.ensure_catalog(core);
                    }
                    Err(e) => {
                        self.failed.push(key);
                        core.toast(
                            ToastKind::Warn,
                            format!("Parameter metadata unavailable: {e}"),
                        );
                    }
                }
            }
        }
    }
}
