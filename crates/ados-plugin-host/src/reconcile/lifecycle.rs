//! Operator lifecycle control and the background loops.

use super::*;

impl<H: HostServices> crate::control::LifecycleControl for PluginReconciler<H> {
    fn rotate_token(&self, plugin_id: &str) -> Result<bool, String> {
        PluginReconciler::rotate_token(self, plugin_id)
    }

    fn reconcile(&self) -> (usize, usize, usize) {
        let report = PluginReconciler::reconcile(self);
        (report.started, report.stopped, report.serving)
    }

    fn purge_data(&self, plugin_id: &str) -> Result<(), String> {
        let installs = state::load_state_checked(Some(&self.state_path))
            .map_err(|e| format!("plugin state is unreadable ({e}); refusing to purge"))?;
        if state::find_install(&installs, plugin_id).is_some() {
            return Err(format!(
                "plugin {plugin_id} is still installed; its data dir is not purged"
            ));
        }
        let mut parts = Path::new(plugin_id).components();
        if !matches!(
            (parts.next(), parts.next()),
            (Some(std::path::Component::Normal(_)), None)
        ) {
            return Err(format!("plugin id {plugin_id:?} does not name a data dir"));
        }
        let dir = self.mint.data_root().join(plugin_id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove {}: {e}", dir.display())),
        }
    }

    fn account(&self, plugin_id: &str, ensure: bool) -> Result<(), String> {
        use crate::plugin_account::PluginAccounts;
        if !crate::manifest::is_plugin_id(plugin_id) {
            return Err(format!("{plugin_id:?} is not a plugin id"));
        }
        let accounts = crate::plugin_account::LocalAccounts;
        if ensure {
            return accounts.ensure(plugin_id).map_err(|e| e.0);
        }
        let installs = state::load_state_checked(Some(&self.state_path))
            .map_err(|e| format!("plugin state is unreadable ({e}); refusing to remove"))?;
        if state::find_install(&installs, plugin_id).is_some() {
            return Err(format!(
                "plugin {plugin_id} is still installed; its user is kept"
            ));
        }
        accounts.remove(plugin_id).map_err(|e| e.0)
    }
}

/// Spawn the two background loops: the state poll and the token rotation.
///
/// Returns their join handles so shutdown can abort them. Both are infinite and
/// unconditional — no attempt cap, no backoff, no terminal state an operator
/// has to clear.
pub fn spawn_loops<H: HostServices + 'static>(
    reconciler: Arc<PluginReconciler<H>>,
) -> (JoinHandle<()>, JoinHandle<()>) {
    let poll_target = reconciler.clone();
    let poll = tokio::spawn(async move {
        // `interval` fires its first tick immediately; start one period out
        // instead. The caller has already run the first pass synchronously, and
        // an immediate duplicate would re-read state for nothing.
        let mut ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let report = poll_target.reconcile();
            poll_target.watch_fc_device().await;
            if report.started > 0 || report.stopped > 0 {
                tracing::info!(
                    started = report.started,
                    stopped = report.stopped,
                    serving = report.serving,
                    "reconciled plugin sockets from state"
                );
            }
        }
    });

    let rotate_target = reconciler;
    let rotate = tokio::spawn(async move {
        // Same offset, and here it is load-bearing rather than tidy: an
        // immediate first tick would rotate tokens that were minted seconds
        // ago in the first reconcile pass, pushing a pointless `token.refresh`
        // at every plugin the instant it connects.
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + ROTATE_INTERVAL,
            ROTATE_INTERVAL,
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let rotated = rotate_target.rotate_all();
            if rotated > 0 {
                tracing::debug!(rotated, "rotated plugin capability tokens ahead of expiry");
            }
        }
    });

    (poll, rotate)
}
