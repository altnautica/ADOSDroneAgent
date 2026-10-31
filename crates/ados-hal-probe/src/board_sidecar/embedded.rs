//! Board profiles compiled into the binary and their YAML parser.

use super::*;

/// The board-profile YAMLs, embedded at compile time.
///
/// Embedded rather than read from disk so the lean headless node — the one this
/// writer exists for — needs nothing but its own binary. [`BOARD_PROFILE_YAML`]
/// is asserted to cover every file in the directory, so adding a board without
/// registering it here fails a test instead of silently detecting as unknown.
pub const BOARD_PROFILE_YAML: &[(&str, &str)] = &[
    ("beagley-ai", include_str!("../../boards/beagley-ai.yaml")),
    ("cm3", include_str!("../../boards/cm3.yaml")),
    ("cm4", include_str!("../../boards/cm4.yaml")),
    ("cm5", include_str!("../../boards/cm5.yaml")),
    ("cubie-a7s", include_str!("../../boards/cubie-a7s.yaml")),
    ("cubie-a7z", include_str!("../../boards/cubie-a7z.yaml")),
    (
        "generic-arm64",
        include_str!("../../boards/generic-arm64.yaml"),
    ),
    (
        "generic-x86_64",
        include_str!("../../boards/generic-x86_64.yaml"),
    ),
    ("jetson-nano", include_str!("../../boards/jetson-nano.yaml")),
    (
        "jetson-orin-nano",
        include_str!("../../boards/jetson-orin-nano.yaml"),
    ),
    ("orange-pi-5", include_str!("../../boards/orange-pi-5.yaml")),
    ("rdk-x3", include_str!("../../boards/rdk-x3.yaml")),
    ("rk3566", include_str!("../../boards/rk3566.yaml")),
    ("rk3576", include_str!("../../boards/rk3576.yaml")),
    ("rk3588s2", include_str!("../../boards/rk3588s2.yaml")),
    (
        "rock-5b-plus",
        include_str!("../../boards/rock-5b-plus.yaml"),
    ),
    (
        "rock-5c-lite",
        include_str!("../../boards/rock-5c-lite.yaml"),
    ),
    ("rpi3", include_str!("../../boards/rpi3.yaml")),
    ("rpi4b", include_str!("../../boards/rpi4b.yaml")),
    ("rpi5", include_str!("../../boards/rpi5.yaml")),
    ("rv1126b", include_str!("../../boards/rv1126b.yaml")),
    ("zero2w", include_str!("../../boards/zero2w.yaml")),
];

/// Parse every embedded profile, in filename order — the same order the Python
/// loader walks (`sorted()` over the directory), which is what makes the
/// first-match scan in [`match_profile`] agree across the two languages.
///
/// A profile that fails to parse, or whose buttons collide with a reserved
/// line, is skipped with a warning rather than taking the whole detection
/// down: one malformed board must not make every board unknown.
pub fn load_profiles() -> Vec<BoardProfile> {
    let mut out = Vec::with_capacity(BOARD_PROFILE_YAML.len());
    for (stem, body) in BOARD_PROFILE_YAML {
        match parse_profile(body) {
            Ok(mut p) => {
                // The filename stem is the canonical board-override token; it
                // is not a YAML key, so it is stamped here.
                p.stem = (*stem).to_string();
                out.push(p);
            }
            Err(e) => tracing::warn!(board = stem, error = %e, "board profile did not parse"),
        }
    }
    out
}

/// Parse and validate one board-profile YAML body.
pub fn parse_profile(body: &str) -> Result<BoardProfile, String> {
    let profile = serde_norway::from_str::<BoardProfile>(body).map_err(|e| e.to_string())?;
    match profile.panel_conflict() {
        Some(conflict) => Err(conflict),
        None => Ok(profile),
    }
}

/// The display ids the resolved board declares, for validating an operator's
/// `--display <id>`. Empty when the board is unknown or declares none, which a
/// caller must treat as "cannot validate", never as "invalid".
pub fn declared_display_ids() -> Vec<String> {
    let profiles = load_profiles();
    let facts = probe_host_facts();
    match resolve_profile(&profiles, &facts) {
        Some(m) => m
            .profile
            .displays
            .supported
            .iter()
            .map(|d| d.id.clone())
            .collect(),
        None => Vec::new(),
    }
}
