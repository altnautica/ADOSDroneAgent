use super::*;
use crate::cli::RunMode;

#[test]
fn validate_pair_code_trims_and_rejects_blank() {
    // A real code is accepted, trimmed of surrounding whitespace.
    assert_eq!(
        validate_pair_code(Some("  ABCD-1234 ")).unwrap(),
        "ABCD-1234"
    );
    assert_eq!(validate_pair_code(Some("code")).unwrap(), "code");
    // Absent or blank → an error (a re-pair with no code must not proceed).
    assert!(validate_pair_code(None).is_err());
    assert!(validate_pair_code(Some("")).is_err());
    assert!(validate_pair_code(Some("   ")).is_err());
}

#[test]
fn pair_only_resolves_only_for_an_installed_box_with_a_bare_code() {
    // A bare pair code resolves to PairOnly only when the install probe says
    // the agent is already on disk; on a fresh box the same code installs.
    let a = Args {
        pair: Some("ABCD-1234".to_string()),
        ..Args::default()
    };
    assert_eq!(RunMode::resolve(&a, true), RunMode::PairOnly);
    assert_eq!(RunMode::resolve(&a, false), RunMode::FreshInstall);
    // --upgrade or --force never short-circuits to a re-pair.
    let up = Args {
        upgrade: true,
        ..a.clone()
    };
    assert_eq!(RunMode::resolve(&up, true), RunMode::Upgrade);
    let force = Args { force: true, ..a };
    assert_eq!(RunMode::resolve(&force, true), RunMode::ForceReinstall);
}
