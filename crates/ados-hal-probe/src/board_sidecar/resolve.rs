//! Tier detection, profile matching and fingerprint resolution.

use super::*;

/// Assign a tier from available RAM. Must agree with `detect_tier` in
/// `src/ados/hal/detect.py`:
/// <512 MB = 1, <2048 = 2, <=4096 = 3, else 4.
pub fn detect_tier(ram_mb: i64) -> i64 {
    if ram_mb < 512 {
        1
    } else if ram_mb < 2048 {
        2
    } else if ram_mb <= 4096 {
        3
    } else {
        4
    }
}

/// The first profile with a pattern contained in `model_string`,
/// case-insensitively. First-match over the filename-ordered list, matching the
/// Python `_match_profile`.
pub fn match_profile<'a>(
    profiles: &'a [BoardProfile],
    model_string: &str,
) -> Option<&'a BoardProfile> {
    if model_string.is_empty() {
        return None;
    }
    let needle = model_string.to_ascii_lowercase();
    profiles.iter().find(|p| {
        p.model_patterns
            .iter()
            .any(|pattern| !pattern.is_empty() && needle.contains(&pattern.to_ascii_lowercase()))
    })
}

/// The variant whose `when` holds for these facts, if any (pure).
///
/// An empty `when` never selects — a catch-all variant would rename every unit
/// of the board silently.
pub fn select_variant(profile: &BoardProfile, cpu_cores: i64) -> Option<&BoardVariant> {
    profile.variants.iter().find(|v| match v.when.cpu_cores {
        Some(n) => n == cpu_cores,
        None => false,
    })
}

/// Resolve the board profile for these facts, mirroring the Python
/// `_resolve_profile_match` exactly.
///
/// Order: the operator's `/etc/ados/board_override` stem, then the device-tree
/// `compatible` first token (which uniquely identifies a board), then the model
/// string (which can be a generic SoC-family name shared by several boards),
/// then `/proc/cpuinfo`.
///
/// An override that names no profile falls THROUGH to auto-detection with a
/// warning rather than minting a profile-less board: the override exists to
/// correct a mis-detection, and a typo must not cost the node its UART
/// candidates and its perception tier.
pub fn resolve_profile<'a>(
    profiles: &'a [BoardProfile],
    facts: &HostFacts,
) -> Option<ProfileMatch<'a>> {
    let detected = [
        facts.model.as_str(),
        facts.compatible.as_str(),
        facts.cpuinfo_model.as_str(),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or("");

    let token = facts.board_override.trim();
    if !token.is_empty() {
        match profiles
            .iter()
            .find(|p| p.stem.eq_ignore_ascii_case(token))
        {
            Some(p) => {
                return Some(ProfileMatch {
                    profile: p,
                    model: if detected.is_empty() {
                        token.to_string()
                    } else {
                        detected.to_string()
                    },
                    source: "override",
                })
            }
            None => tracing::warn!(
                token,
                "board_override names no board profile (expected a YAML filename stem, e.g. rock-5c-lite); auto-detecting"
            ),
        }
    }

    if let Some(p) = match_profile(profiles, &facts.compatible) {
        return Some(ProfileMatch {
            profile: p,
            model: if facts.model.is_empty() {
                facts.compatible.clone()
            } else {
                facts.model.clone()
            },
            source: "compatible",
        });
    }
    if let Some(p) = match_profile(profiles, &facts.model) {
        return Some(ProfileMatch {
            profile: p,
            model: facts.model.clone(),
            source: "model",
        });
    }
    if let Some(p) = match_profile(profiles, &facts.cpuinfo_model) {
        return Some(ProfileMatch {
            profile: p,
            model: facts.cpuinfo_model.clone(),
            source: "cpuinfo",
        });
    }
    None
}

/// Build the fingerprint from a matched profile, keeping the detected model
/// string and applying the matching same-device-tree variant. Mirrors the
/// Python `_board_from_profile`.
pub(super) fn from_profile(
    profile: &BoardProfile,
    model_string: &str,
    facts: &HostFacts,
) -> BoardFingerprint {
    let local_inference = profile
        .compute
        .local_inference
        .clone()
        .unwrap_or_else(|| "none".to_string());
    let variant = select_variant(profile, facts.cpu_cores);
    let name = variant
        .and_then(|v| v.name.clone())
        .unwrap_or_else(|| profile.name.clone());
    let soc = variant
        .and_then(|v| v.soc.clone())
        .unwrap_or_else(|| profile.soc.clone());
    let tier = variant
        .and_then(|v| v.default_tier)
        .unwrap_or(profile.default_tier);
    BoardFingerprint {
        version: BOARD_SIDECAR_VERSION,
        model: if model_string.is_empty() {
            name.clone()
        } else {
            model_string.to_string()
        },
        name,
        profile: profile.stem.clone(),
        tier,
        ram_mb: facts.ram_mb,
        cpu_cores: facts.cpu_cores,
        vendor: profile.vendor.clone(),
        soc,
        arch: profile.arch.clone(),
        hw_video_codecs: profile.hw_video_codecs.clone(),
        npu_tops: profile.compute.npu_tops,
        has_accelerator: profile.compute.npu_tops > 0.0,
        has_local_inference: !matches!(local_inference.as_str(), "" | "none"),
        local_inference,
        buttons: profile.buttons,
        encoder: profile.encoder,
        sensor_hub: crate::sensor_hub::detect(profile.sensor_hub.as_ref(), &facts.usb_ttys),
        battery_gauge: profile.battery_gauge.clone(),
        reserved_gpio: profile.reserved_lines(),
    }
}

/// The generic profile stem for an architecture, and the name an unmatched
/// board is published under. Kept together because the Python half resolves the
/// identical pair — the two used to disagree (Rust resolved through
/// `generic-arm64`, Python minted a bare `generic-aarch64` with no profile and
/// therefore no fallback UART candidates).
fn generic_identity(facts: &HostFacts) -> (&'static str, String) {
    let stem = if facts.machine == "x86_64" || facts.machine == "AMD64" {
        "generic-x86_64"
    } else {
        "generic-arm64"
    };
    let name = if facts.os == "macos" {
        // A dev Mac is not a board; say so rather than claiming an SBC profile.
        "macOS (dev)".to_string()
    } else {
        stem.to_string()
    };
    (stem, name)
}

/// Resolve the board fingerprint from host facts.
///
/// Detection order is [`resolve_profile`]'s. An unmatched board resolves
/// through the generic profile for its architecture so it keeps that profile's
/// declarations (the fallback UART candidates, the camera defaults) instead of
/// nothing, and is tiered from the RAM it actually has.
pub fn resolve(profiles: &[BoardProfile], facts: &HostFacts) -> BoardFingerprint {
    if let Some(m) = resolve_profile(profiles, facts) {
        return from_profile(m.profile, &m.model, facts);
    }

    let (generic_stem, generic_name) = generic_identity(facts);
    let detected_model = [
        facts.model.as_str(),
        facts.cpuinfo_model.as_str(),
        facts.compatible.as_str(),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or("")
    .to_string();
    if let Some(p) = profiles.iter().find(|p| p.stem == generic_stem) {
        let mut fp = from_profile(p, &detected_model, facts);
        fp.name = generic_name;
        if fp.model.is_empty() {
            fp.model = fp.name.clone();
        }
        // The generic profile's default_tier is a placeholder; an unknown board
        // is tiered from what it actually has.
        fp.tier = detect_tier(facts.ram_mb);
        return fp;
    }
    BoardFingerprint {
        version: BOARD_SIDECAR_VERSION,
        model: if detected_model.is_empty() {
            generic_name.clone()
        } else {
            detected_model
        },
        name: generic_name,
        profile: String::new(),
        tier: detect_tier(facts.ram_mb),
        ram_mb: facts.ram_mb,
        cpu_cores: facts.cpu_cores,
        vendor: "unknown".to_string(),
        soc: "unknown".to_string(),
        arch: if facts.machine.is_empty() {
            "aarch64".to_string()
        } else {
            facts.machine.clone()
        },
        hw_video_codecs: Vec::new(),
        npu_tops: 0.0,
        has_accelerator: false,
        local_inference: "none".to_string(),
        has_local_inference: false,
        buttons: None,
        encoder: None,
        sensor_hub: crate::sensor_hub::detect(None, &facts.usb_ttys),
        battery_gauge: None,
        reserved_gpio: Vec::new(),
    }
}
