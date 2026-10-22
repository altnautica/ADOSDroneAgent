//! Runtime paths of the LoRa lane. Run-dir files resolve under the
//! `ADOS_RUN_DIR` override through `ados_protocol::paths::run_dir`.

use std::path::PathBuf;

/// The lane state sidecar: the `GET /api/v1/lora` body minus `present`.
pub const LORA_JSON: &str = "/run/ados/lora.json";

/// Beacons heard on the lane, newest first: the `GET /api/v1/lora/beacons` body.
pub const LORA_BEACONS_JSON: &str = "/run/ados/lora-beacons.json";

/// Schema version of both sidecars, kept equal to the `contracts.toml` rows.
pub const LORA_SIDECAR_VERSION: u16 = 1;

/// The lane state sidecar path, honouring `ADOS_RUN_DIR`.
pub fn lora_json() -> PathBuf {
    ados_protocol::paths::run_dir().join(ados_protocol::paths::LORA_JSON)
}

/// The beacons sidecar path, honouring `ADOS_RUN_DIR`.
pub fn lora_beacons_json() -> PathBuf {
    ados_protocol::paths::run_dir().join(ados_protocol::paths::LORA_BEACONS_JSON)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_versions_match_the_registry() {
        assert_eq!(
            Some(LORA_SIDECAR_VERSION),
            ados_protocol::contracts::sidecar_version("lora")
        );
        assert_eq!(
            Some(LORA_SIDECAR_VERSION),
            ados_protocol::contracts::sidecar_version("lora-beacons")
        );
    }
}
