use super::identity::default_name;
use super::review::{friendly_profile, review_summary};
use super::role::{catalog_body, pack_labels, set_profile, world_engine_item};
use super::wifi_flow::short_reason;
use super::*;
use crate::steps::config_identity::slugify_hostname;
use crate::wizard::catalog;

#[test]
fn default_names_are_hostname_safe() {
    assert_eq!(default_name("drone"), "ados-drone-01");
    assert_eq!(default_name("ground_station"), "ados-ground-01");
    assert_eq!(default_name("workstation"), "ados-workstation-01");
    // The seed must already be a valid slug (no change under slugify).
    for p in ["drone", "ground_station", "workstation", "compute"] {
        let seed = default_name(p);
        assert_eq!(
            slugify_hostname(&seed),
            seed,
            "seed not slug-stable: {seed}"
        );
    }
}

#[test]
fn short_reason_trims_and_bounds() {
    assert_eq!(
        short_reason("Error: Secrets were required"),
        "Secrets were required"
    );
    assert_eq!(short_reason(""), "could not connect");
    let long = "x".repeat(80);
    let out = short_reason(&long);
    assert!(out.chars().count() <= 49, "reason not bounded: {out}");
}

#[test]
fn hardware_frame_fits_and_is_context_aware_per_profile() {
    use ados_term::frame::{compose, Chrome, Screen, TermSize};
    use ados_term::theme::Theme;

    let theme = Theme::detect(true, true); // no color, ASCII — readable + portable
    let sys = hw::SysProbe::default(); // nothing attached: every Now row reads "not detected"
    let size = TermSize { cols: 90, rows: 30 };
    for (profile, id, want, forbid) in [
        (
            catalog::Profile::Drone,
            "drone",
            "Flight controller",
            "HDMI output",
        ),
        (
            catalog::Profile::GroundStation,
            "ground_station",
            "Long-range radio",
            "Flight controller",
        ),
        (
            catalog::Profile::Workstation,
            "workstation",
            "This computer",
            "Long-range radio",
        ),
        (
            catalog::Profile::Visor,
            "visor",
            "Sensor hub",
            "Long-range radio",
        ),
    ] {
        let body = catalog_body(&theme, profile, &sys);
        let screen = Screen {
            section: SETUP,
            body: &body,
            footer: "Enter to continue",
        };
        let chrome = Chrome {
            step: 2,
            total: 6,
            label: "Hardware".to_string(),
        };
        let grid = compose(&theme, &chrome, &screen, size);
        // Every row fits the terminal width exactly (no overflow off-screen).
        for (i, line) in grid.iter().enumerate() {
            assert_eq!(
                line.chars().count(),
                90,
                "{id}: row {i} not 90 cols: {line:?}"
            );
        }
        let joined = grid.join("\n");
        // The whole body lands on screen (nothing clipped past the footer): the
        // last catalog label must be present.
        assert!(joined.contains(want), "{id}: missing expected {want:?}");
        // Context-awareness: the other profile's hardware must not appear.
        assert!(
            !joined.contains(forbid),
            "{id}: leaked another profile's {forbid:?}"
        );
        // Uncomment locally to eyeball the layout: `cargo test hardware_frame -- --nocapture`.
        // eprintln!("\n==== {id} ====\n{joined}");
    }
}

#[test]
fn pack_labels_wraps_within_width_and_covers_all() {
    let labels = [
        "Camera gimbal",
        "Distance sensor",
        "Optical flow",
        "LiDAR",
        "mmWave radar",
        "RTK GNSS",
    ];
    let lines = pack_labels(&labels, 30);
    // Every line is within the width.
    for l in &lines {
        assert!(l.chars().count() <= 30, "line too wide: {l:?}");
    }
    // Every label survives, in order, across the joined lines.
    let joined = lines.join(", ");
    for l in labels {
        assert!(joined.contains(l), "lost label {l}");
    }
    assert!(
        lines.len() > 1,
        "should wrap into multiple lines at width 30"
    );
    // A single label longer than the width still emits (not dropped).
    assert_eq!(pack_labels(&["a-very-long-single-label"], 5).len(), 1);
    assert!(pack_labels(&[], 40).is_empty());
}

#[test]
fn visible_steps_omit_pairing_for_a_workstation() {
    let drone = Args {
        profile: Some("drone".into()),
        ..Args::default()
    };
    assert!(
        visible_steps(&drone).contains(&Step::Pair),
        "a drone should walk the pairing step"
    );
    let gs = Args {
        profile: Some("ground_station".into()),
        ..Args::default()
    };
    assert!(visible_steps(&gs).contains(&Step::Pair));
    for p in ["workstation", "compute"] {
        let ws = Args {
            profile: Some(p.into()),
            ..Args::default()
        };
        let steps = visible_steps(&ws);
        assert!(
            !steps.contains(&Step::Pair),
            "{p} must not walk the pairing step"
        );
        // Review is always the terminal step.
        assert_eq!(steps.last(), Some(&Step::Review));
    }
}

#[test]
fn the_world_engine_row_defaults_per_profile_and_keeps_an_answer() {
    let with = |profile: &str, choice: Option<bool>| Args {
        profile: Some(profile.into()),
        world_engine: choice,
        ..Args::default()
    };
    for (profile, on) in [
        ("workstation", true),
        ("compute", true),
        ("drone", false),
        ("ground_station", false),
    ] {
        let item = world_engine_item(&with(profile, None), profile);
        assert_eq!(item.checked, on, "{profile} default");
        assert!(!item.locked, "{profile}: the operator can always change it");
    }
    // An answer already given (a flag, or an earlier pass) is what shows.
    assert!(world_engine_item(&with("drone", Some(true)), "drone").checked);
    assert!(!world_engine_item(&with("workstation", Some(false)), "workstation").checked);
}

#[test]
fn changing_the_profile_resets_the_world_engine_answer() {
    // The first pick keeps a flag the operator passed.
    let mut args = Args {
        world_engine: Some(true),
        ..Args::default()
    };
    set_profile(&mut args, "drone");
    assert_eq!(args.world_engine, Some(true));
    // Re-picking the same profile keeps the answer.
    set_profile(&mut args, "drone");
    assert_eq!(args.world_engine, Some(true));
    // A different profile offers its own default again.
    args.world_engine = Some(false);
    set_profile(&mut args, "workstation");
    assert_eq!(args.world_engine, None);
    assert_eq!(args.profile.as_deref(), Some("workstation"));
}

#[test]
fn the_review_names_the_world_engine_choice() {
    let theme = Theme::detect(true, true);
    let rows = |args: &Args| {
        review_summary(
            &theme,
            args,
            &WizardExtras::default(),
            &Collected::default(),
        )
        .join("\n")
    };
    let ws = Args {
        profile: Some("workstation".into()),
        ..Args::default()
    };
    assert!(rows(&ws).contains("World Engine"));
    let drone = Args {
        profile: Some("drone".into()),
        ..Args::default()
    };
    assert!(!rows(&drone).contains("World Engine"));
}

#[test]
fn friendly_profile_names() {
    assert_eq!(friendly_profile("drone"), "Drone");
    assert_eq!(friendly_profile("ground_station"), "Ground station");
    assert_eq!(friendly_profile("workstation"), "Workstation");
    assert_eq!(friendly_profile("anything-else"), "Drone");
}
