//! Apply CloudChoiceRequest to agent.yaml.
//!
//! Three modes — cloud (Altnautica-hosted relay, the default), self_hosted
//! (operator-supplied Convex + MQTT coordinates), and local (no cloud at
//! all). The `api_key` field, when supplied, never appears in any
//! response or log line; it lands in `agent.yaml` cloud.api_key. Future
//! future enhancement: split secrets into a separate root-owned 0600
//! file under /etc/ados/secrets/ and reference them by path from
//! agent.yaml so the YAML itself can stay 0644.

use std::path::Path;

use serde_yaml::Value;
use thiserror::Error;

use crate::models::SelfHostedBackend;

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("invalid mode: {0}")]
    InvalidMode(String),

    #[error("self_hosted block required for self_hosted mode")]
    MissingSelfHosted,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

const VALID_MODES: &[&str] = &["cloud", "self_hosted", "local"];
const ALTNAUTICA_RELAY: &str = "https://convex-site.altnautica.com";

pub fn apply_cloud_choice(
    agent_yaml: &Path,
    mode: &str,
    self_hosted: Option<&SelfHostedBackend>,
) -> Result<(), CloudError> {
    if !VALID_MODES.contains(&mode) {
        return Err(CloudError::InvalidMode(mode.to_string()));
    }
    if mode == "self_hosted" && self_hosted.is_none() {
        return Err(CloudError::MissingSelfHosted);
    }

    let mut doc = if agent_yaml.exists() {
        let raw = std::fs::read_to_string(agent_yaml)?;
        if raw.trim().is_empty() {
            Value::Mapping(Default::default())
        } else {
            serde_yaml::from_str(&raw)?
        }
    } else {
        Value::Mapping(Default::default())
    };
    if !doc.is_mapping() {
        doc = Value::Mapping(Default::default());
    }
    let map = doc.as_mapping_mut().expect("doc is mapping");

    let cloud_key = Value::String("cloud".into());
    let cloud = map
        .entry(cloud_key)
        .or_insert_with(|| Value::Mapping(Default::default()));
    if !cloud.is_mapping() {
        *cloud = Value::Mapping(Default::default());
    }
    let cloud_map = cloud.as_mapping_mut().expect("cloud is mapping");

    cloud_map.insert(
        Value::String("mode".into()),
        Value::String(mode.into()),
    );

    match mode {
        "cloud" => {
            cloud_map.insert(
                Value::String("convex_url".into()),
                Value::String(ALTNAUTICA_RELAY.into()),
            );
            // mqtt_broker stays whatever was set; pairing flow populates it.
        }
        "self_hosted" => {
            let sh = self_hosted.expect("checked above");
            cloud_map.insert(
                Value::String("convex_url".into()),
                Value::String(sh.url.clone()),
            );
            if !sh.mqtt_broker.is_empty() {
                cloud_map.insert(
                    Value::String("mqtt_broker".into()),
                    Value::String(sh.mqtt_broker.clone()),
                );
            }
            cloud_map.insert(
                Value::String("mqtt_port".into()),
                Value::Number(serde_yaml::Number::from(sh.mqtt_port)),
            );
            if !sh.api_key.is_empty() {
                cloud_map.insert(
                    Value::String("api_key".into()),
                    Value::String(sh.api_key.clone()),
                );
            }
        }
        "local" => {
            // Wipe the cloud connection details. The agent will run
            // offline. We leave api_key alone so re-engaging the cloud
            // later does not require a fresh pairing if the operator only
            // wanted to temporarily disable.
            cloud_map.insert(
                Value::String("convex_url".into()),
                Value::String("".into()),
            );
            cloud_map.insert(
                Value::String("mqtt_broker".into()),
                Value::String("".into()),
            );
        }
        _ => unreachable!(),
    }

    let serialized = serde_yaml::to_string(&doc)?;
    crate::atomic::atomic_write(agent_yaml, serialized.as_bytes(), 0o640)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_mode_writes_altnautica_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "agent:\n  device_id: \"x\"\n").unwrap();
        apply_cloud_choice(&path, "cloud", None).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: Value = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            doc.get("cloud")
                .and_then(|c| c.get("convex_url"))
                .and_then(|v| v.as_str()),
            Some(ALTNAUTICA_RELAY)
        );
        assert_eq!(
            doc.get("cloud")
                .and_then(|c| c.get("mode"))
                .and_then(|v| v.as_str()),
            Some("cloud")
        );
    }

    #[test]
    fn self_hosted_persists_url_and_creds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "agent:\n  device_id: \"x\"\n").unwrap();
        let sh = SelfHostedBackend {
            url: "https://relay.example".into(),
            mqtt_broker: "broker.example".into(),
            mqtt_port: 1883,
            api_key: "secret".into(),
        };
        apply_cloud_choice(&path, "self_hosted", Some(&sh)).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: Value = serde_yaml::from_str(&raw).unwrap();
        let cloud = doc.get("cloud").unwrap();
        assert_eq!(cloud.get("convex_url").unwrap().as_str(), Some("https://relay.example"));
        assert_eq!(cloud.get("mqtt_broker").unwrap().as_str(), Some("broker.example"));
        assert_eq!(cloud.get("api_key").unwrap().as_str(), Some("secret"));
    }

    #[test]
    fn local_mode_blanks_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(
            &path,
            "agent:\n  device_id: \"x\"\ncloud:\n  convex_url: \"https://x.example\"\n  mqtt_broker: \"b\"\n",
        )
        .unwrap();
        apply_cloud_choice(&path, "local", None).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: Value = serde_yaml::from_str(&raw).unwrap();
        let cloud = doc.get("cloud").unwrap();
        assert_eq!(cloud.get("convex_url").unwrap().as_str(), Some(""));
        assert_eq!(cloud.get("mqtt_broker").unwrap().as_str(), Some(""));
    }

    #[test]
    fn invalid_mode_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        let err = apply_cloud_choice(&path, "tealf", None).unwrap_err();
        assert!(matches!(err, CloudError::InvalidMode(_)));
    }

    #[test]
    fn self_hosted_without_block_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        let err = apply_cloud_choice(&path, "self_hosted", None).unwrap_err();
        assert!(matches!(err, CloudError::MissingSelfHosted));
    }
}
