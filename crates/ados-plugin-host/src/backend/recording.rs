//! An in-memory backend that records every verb.

use super::*;

impl Default for RecordingBackend {
    fn default() -> Self {
        RecordingBackend {
            calls: Mutex::new(Vec::new()),
            units: Mutex::new(BTreeMap::new()),
            sandbox: true,
            on_enable_start: None,
        }
    }
}

impl RecordingBackend {
    /// A recorder that reports no sandbox, as launchd does.
    pub fn without_sandbox() -> Self {
        RecordingBackend {
            sandbox: false,
            ..RecordingBackend::default()
        }
    }

    /// Run `hook` with the unit name at every `enable_start`, the moment a
    /// real service manager would exec the unit, so a test can check what
    /// exists by then.
    pub fn on_enable_start(mut self, hook: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_enable_start = Some(Box::new(hook));
        self
    }

    /// Every `(verb, unit name)` recorded, in order.
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().clone()
    }

    /// Whether `verb` was recorded for `name`.
    pub fn called(&self, verb: &str, name: &str) -> bool {
        self.calls().iter().any(|(v, n)| v == verb && n == name)
    }

    /// Forget the recorded calls.
    pub fn clear_calls(&self) {
        self.calls.lock().clear();
    }

    /// The rendered systemd text of an installed unit.
    pub fn unit(&self, name: &str) -> Option<String> {
        self.units.lock().get(name).cloned()
    }

    fn record(&self, verb: &str, name: &str) {
        self.calls.lock().push((verb.to_string(), name.to_string()));
    }
}

impl ServiceBackend for RecordingBackend {
    fn install(&self, name: &str, spec: &UnitSpec) -> Result<bool, SupervisorError> {
        self.record("install", name);
        let text = render_systemd(spec);
        let previous = self.units.lock().insert(name.to_string(), text.clone());
        Ok(previous.as_ref() != Some(&text))
    }

    fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
        self.record("uninstall", name);
        self.units.lock().remove(name);
        Ok(())
    }

    fn enable_start(&self, name: &str) -> Result<(), SupervisorError> {
        self.record("enable_start", name);
        if let Some(hook) = &self.on_enable_start {
            hook(name);
        }
        Ok(())
    }

    fn stop_disable(&self, name: &str) -> Result<(), SupervisorError> {
        self.record("stop_disable", name);
        Ok(())
    }

    fn restart(&self, name: &str) -> Result<(), SupervisorError> {
        self.record("restart", name);
        Ok(())
    }

    fn is_active(&self, name: &str) -> Result<bool, SupervisorError> {
        self.record("is_active", name);
        Ok(true)
    }

    fn probe(&self, _probe: &ProbeSpec) -> Result<(), SupervisorError> {
        self.record("probe", "");
        Ok(())
    }

    fn enforces_sandbox(&self) -> bool {
        self.sandbox
    }
}
