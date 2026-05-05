//! Request and response shapes mirroring the Python reference at
//! `src/ados/setup/models.py`. These are wire-compatible — JSON
//! serialization must match the Python output byte-for-byte for the
//! conformance suite to pass.

use serde::{Deserialize, Serialize};

/// Canonical step ids the wizard emits. Used to validate skip targets.
pub const VALID_STEP_IDS: &[&str] = &[
    "welcome",
    "profile",
    "hardware_check",
    "cloud_choice",
    "pair",
    "mavlink",
    "video",
    "ground_receiver",
    "remote_access",
    "finish",
];

/// Steps that cannot be skipped — the wizard requires them.
pub const REQUIRED_STEP_IDS: &[&str] = &["welcome", "finish"];

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileChoiceRequest {
    pub profile: String, // "drone" | "ground_station"
    #[serde(default)]
    pub ground_role: Option<String>, // "direct" | "relay" | "receiver"
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudChoiceRequest {
    pub mode: String, // "cloud" | "self_hosted" | "local"
    #[serde(default)]
    pub self_hosted: Option<SelfHostedBackend>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SelfHostedBackend {
    pub url: String,
    #[serde(default)]
    pub mqtt_broker: String,
    #[serde(default = "default_mqtt_port")]
    pub mqtt_port: u16,
    #[serde(default)]
    pub api_key: String,
}

fn default_mqtt_port() -> u16 {
    8883
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudflareTokenRequest {
    pub token_or_script: String,
}

/// Generic action-result shape returned by mutation routes. The Python
/// reference returns `{ ok, message, status }` with `status` carrying the
/// updated SetupStatus.
#[derive(Debug, Clone, Serialize)]
pub struct SetupActionResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub status: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudflareVerifyResponse {
    pub reachable: bool,
    pub status_code: Option<u16>,
    pub latency_ms: Option<u64>,
    pub target_url: Option<String>,
    pub error: Option<String>,
}

/// One row in the hardware-check step's per-component readout. Mirrors
/// HardwareCheckItem at src/ados/setup/models.py byte-for-byte.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HardwareCheckItem {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
    pub state: String, // "ok" | "missing" | "warning" | "checking" | "unknown"
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub fix_hint: String,
}

impl HardwareCheckItem {
    pub fn new(id: &str, label: &str) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            required: false,
            state: "unknown".into(),
            detail: String::new(),
            fix_hint: String::new(),
        }
    }

    pub fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }

    pub fn ok(mut self, detail: impl Into<String>) -> Self {
        self.state = "ok".into();
        self.detail = detail.into();
        self
    }

    pub fn missing(mut self, detail: impl Into<String>, fix_hint: impl Into<String>) -> Self {
        self.state = "missing".into();
        self.detail = detail.into();
        self.fix_hint = fix_hint.into();
        self
    }

    pub fn warning(mut self, detail: impl Into<String>, fix_hint: impl Into<String>) -> Self {
        self.state = "warning".into();
        self.detail = detail.into();
        self.fix_hint = fix_hint.into();
        self
    }

    pub fn unknown(mut self, detail: impl Into<String>) -> Self {
        self.state = "unknown".into();
        self.detail = detail.into();
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HardwareCheckStatus {
    pub profile: String,
    #[serde(default)]
    pub ground_role: String,
    pub items: Vec<HardwareCheckItem>,
    pub last_run: String,
}
