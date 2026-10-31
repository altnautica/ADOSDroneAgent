use serde_json::json;

use super::*;

fn with_kiosk(url: Option<&str>, minimal: Option<bool>) -> Value {
    json!({"ground_station": {"kiosk": {"target_url": url, "minimal_layer": minimal}}})
}

fn with_settings(enabled: bool, resolution: &str) -> Value {
    json!({"ground_station": {
        "display": {"type": "auto"},
        "kiosk": {"enabled": enabled, "resolution": resolution},
    }})
}

#[test]
fn default_cockpit_url_is_the_served_path() {
    assert_eq!(DEFAULT_URL, "http://localhost:8080/cockpit/?kiosk=1");
}

#[test]
fn kiosk_config_missing_or_empty_section_is_unset() {
    assert_eq!(kiosk_config(&json!({})), (None, None));
    assert_eq!(
        kiosk_config(&json!({"ground_station": {"kiosk": null}})),
        (None, None)
    );
}

#[test]
fn kiosk_config_blank_url_reads_as_unset() {
    assert_eq!(
        kiosk_config(&with_kiosk(Some("   "), Some(true))),
        (None, Some(true))
    );
    assert_eq!(
        kiosk_config(&with_kiosk(Some("http://example/hud"), Some(false))),
        (Some("http://example/hud".into()), Some(false))
    );
}

#[test]
fn target_url_defaults_to_the_minimal_cockpit() {
    assert_eq!(
        resolve_target_url(&json!({}), None, None),
        (
            "http://localhost:8080/cockpit/?kiosk=1&layer=minimal".into(),
            true
        )
    );
}

#[test]
fn target_url_env_then_config_precedence() {
    assert_eq!(
        resolve_target_url(&json!({}), Some("http://env-host/hud"), None).0,
        "http://env-host/hud?layer=minimal"
    );
    let cfg = with_kiosk(Some("http://cfg-host/hud"), None);
    assert_eq!(
        resolve_target_url(&cfg, Some("http://env-host/hud"), None).0,
        "http://cfg-host/hud?layer=minimal"
    );
}

#[test]
fn only_an_explicit_false_selects_the_full_layer() {
    let (url, minimal) = resolve_target_url(
        &with_kiosk(Some("http://host/hud"), Some(false)),
        None,
        None,
    );
    assert!(!minimal);
    assert!(!url.contains("layer=minimal"));
    let (url, minimal) = resolve_target_url(
        &with_kiosk(Some("http://host/hud?theme=dark"), Some(true)),
        None,
        None,
    );
    assert!(minimal);
    assert_eq!(url, "http://host/hud?theme=dark&layer=minimal");
    let (url, minimal) = resolve_target_url(&json!({}), None, Some("0"));
    assert!(!minimal);
    assert!(!url.contains("layer=minimal"));
}

#[test]
fn kiosk_settings_map_resolution_presets_and_leave_auto_alone() {
    assert_eq!(
        kiosk_settings(&with_settings(true, "1080p")),
        KioskSettings {
            enabled: true,
            output_mode: Some("1920x1080")
        }
    );
    assert_eq!(
        kiosk_settings(&with_settings(true, "720p")).output_mode,
        Some("1280x720")
    );
    for value in ["auto", "4k", ""] {
        assert_eq!(
            kiosk_settings(&with_settings(true, value)).output_mode,
            None
        );
    }
    // A config without the section reads as disabled.
    assert!(!kiosk_settings(&json!({})).enabled);
}

#[test]
fn display_selection_reads_the_operator_choice() {
    for (raw, expected) in [
        ("hdmi", "hdmi"),
        ("LCD", "lcd"),
        (" none ", "none"),
        ("auto", "auto"),
    ] {
        let cfg = json!({"ground_station": {"display": {"type": raw}}});
        assert_eq!(display_selection(&cfg), expected);
    }
}

#[test]
fn display_selection_falls_back_to_auto_on_anything_unusable() {
    assert_eq!(display_selection(&json!({"logging": {}})), "auto");
    assert_eq!(
        display_selection(&json!({"ground_station": {"display": {"type": null}}})),
        "auto"
    );
    assert_eq!(
        display_selection(&json!({"ground_station": {"display": {"type": "banana"}}})),
        "auto"
    );
}

#[test]
fn the_effective_config_enables_the_kiosk_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = load_config(&dir.path().join("absent.yaml")).unwrap();
    let settings = kiosk_settings(&cfg);
    assert!(settings.enabled);
    assert_eq!(settings.output_mode, None);
    assert_eq!(display_selection(&cfg), "auto");
}

#[test]
fn watch_reports_only_a_change_of_kiosk_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    let mut watch = SettingsWatch::new(
        path.clone(),
        KioskSettings {
            enabled: true,
            output_mode: None,
        },
        None,
    );
    // Unchanged signature (still absent): no reload.
    assert!(!watch.poll());
    // A new file whose kiosk settings match: signature moves, no change.
    std::fs::write(&path, "ground_station:\n  kiosk:\n    enabled: true\n").unwrap();
    assert!(!watch.poll());
    // A replaced file with a new resolution: a change.
    let tmp = dir.path().join("config.yaml.tmp");
    std::fs::write(&tmp, "ground_station:\n  kiosk:\n    resolution: 1080p\n").unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    assert!(watch.poll());
    assert_eq!(watch.settings.output_mode, Some("1920x1080"));
}
