//! The one ground-station profile resolution rule every plane shares.

use serde::Deserialize;

/// The profile-source sentinel install writes and `ados profile set` flips. The
/// resolver below consults it whenever `agent.profile` is `auto`, empty or absent.
pub const PROFILE_CONF: &str = "/etc/ados/profile.conf";

/// True when the agent profile resolves to a ground station. Reads
/// `agent.profile` from the config file, falling back to `profile.conf`; see
/// [`ground_station_from_config_text`] for the rules. The WFB TX service idles
/// there (the GS runs `ados-wfb-rx`, not this) so it doesn't clobber the GS's own
/// `wfb-stats.json`, and the swarm bus listens without transmitting.
pub fn profile_is_ground_station(
    config_path: &std::path::Path,
    profile_conf: &std::path::Path,
) -> bool {
    let text = std::fs::read_to_string(config_path).ok();
    ground_station_from_config_text(text.as_deref(), profile_conf)
}

/// The one ground-station resolution rule every plane shares, over already-read
/// config text so a caller that parsed the file for its own keys resolves the
/// profile from the same bytes.
///
/// An explicit `agent.profile` of `ground_station` / `ground-station` is a ground
/// station and `drone` is not. `auto`, empty, absent, or an unparseable config
/// defer to `profile.conf`, whose `profile:` / `profile=` line decides; with no
/// usable line the node is not a ground station.
pub fn ground_station_from_config_text(
    config_text: Option<&str>,
    profile_conf: &std::path::Path,
) -> bool {
    #[derive(Debug, Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        agent: AgentSection,
    }
    #[derive(Debug, Default, Deserialize)]
    struct AgentSection {
        #[serde(default)]
        profile: Option<String>,
    }
    let cfg_profile = config_text
        .and_then(|t| serde_norway::from_str::<Raw>(t).ok())
        .and_then(|r| r.agent.profile);
    match cfg_profile.as_deref() {
        Some("ground_station") | Some("ground-station") => return true,
        Some("drone") => return false,
        _ => {} // empty/auto/missing → consult profile.conf
    }
    if let Ok(text) = std::fs::read_to_string(profile_conf) {
        for line in text.lines() {
            let s = line.trim();
            if let Some(v) = s
                .strip_prefix("profile:")
                .or_else(|| s.strip_prefix("profile="))
            {
                let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                return matches!(v, "ground_station" | "ground-station");
            }
        }
    }
    false
}
