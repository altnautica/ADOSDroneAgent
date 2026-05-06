//! Argv composer round-trips.
//!
//! Pins the byte-for-byte argv layout the [`ados_wfb::WfbTxArgs::to_argv`]
//! composer produces. The agent shells out to the upstream `wfb_tx` C
//! binary; any silent reorder or rename of the short-flag pairs would
//! change the over-the-air parameters in production with no other warning,
//! so the argv contract is pinned by these tests.
//!
//! Three vectors:
//!
//! 1. Non-default config: every operator-controlled tunable is set away
//!    from its default (channel 11, MCS 4, tx-power 24, FEC 4/8, radio
//!    port 2, bandwidth 40, short guard, STBC 1, LDPC 1, UDP 5601). The
//!    expected argv vector is asserted index-by-index plus pair-membership
//!    so a future re-order of the flag list surfaces as a hard failure.
//! 2. Default config: every advanced opt left untouched. The argv must
//!    carry the documented WFB-ng example-config defaults verbatim.
//! 3. Custom keypair path: a non-default path threads through `-K` exactly
//!    as stored, including spaces and unicode bytes that would break a
//!    naïve shell-quote helper.

use std::path::PathBuf;

use ados_wfb::{
    GuardInterval, WfbAdvancedOpts, WfbConfig, WfbManager, WfbTxArgs, DEFAULT_KEYPAIR_PATH,
    DEFAULT_WFB_TX_PATH,
};

/// Compose argv straight from a [`WfbTxArgs`] so the test does not have
/// to bring up a manager + drive a dongle event when all that is being
/// asserted is the composer's output.
fn argv_for(args: &WfbTxArgs) -> Vec<String> {
    args.to_argv()
}

/// Build a non-default `WfbTxArgs` matching the test parameters.
fn non_default_args(keypair_path: PathBuf) -> WfbTxArgs {
    WfbTxArgs {
        interface: "wlan0".to_string(),
        channel: 11,
        mcs_index: 4,
        tx_power_dbm: 24,
        keypair_path,
        advanced: WfbAdvancedOpts {
            fec_k: 4,
            fec_n: 8,
            radio_port: 2,
            bandwidth_mhz: 40,
            guard_interval: GuardInterval::Short,
            stbc: 1,
            ldpc: 1,
            udp_listen_port: 5601,
        },
    }
}

/// Helper that locates a `-X <value>` pair in argv and returns the value.
fn find_flag<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|s| s == flag)
        .and_then(|i| argv.get(i + 1).map(|s| s.as_str()))
}

#[test]
fn argv_carries_every_non_default_tunable() {
    let kp = PathBuf::from("/etc/ados/secrets/wfb-keypair");
    let args = non_default_args(kp.clone());
    let argv = argv_for(&args);

    // Every documented short flag pair must land in the argv.
    assert_eq!(find_flag(&argv, "-K"), Some(kp.to_string_lossy().as_ref()));
    assert_eq!(find_flag(&argv, "-k"), Some("4"));
    assert_eq!(find_flag(&argv, "-n"), Some("8"));
    assert_eq!(find_flag(&argv, "-p"), Some("2"));
    assert_eq!(find_flag(&argv, "-B"), Some("40"));
    assert_eq!(find_flag(&argv, "-G"), Some("short"));
    assert_eq!(find_flag(&argv, "-M"), Some("4"));
    assert_eq!(find_flag(&argv, "-S"), Some("1"));
    assert_eq!(find_flag(&argv, "-L"), Some("1"));
    assert_eq!(find_flag(&argv, "-u"), Some("5601"));

    // Trailing positional is the wlan interface.
    assert_eq!(argv.last().map(String::as_str), Some("wlan0"));

    // Channel + tx-power are NOT in the argv (the orchestration layer
    // sets them via `iw` before exec). Pin that contract here so a
    // future drift toward "stuff every field into argv" surfaces.
    assert!(!argv.iter().any(|s| s == "11"));
    assert!(!argv.iter().any(|s| s == "24"));
}

#[test]
fn argv_layout_is_byte_for_byte_stable_for_non_defaults() {
    // Pin the exact ordering the composer emits today. Any future change
    // to the layout — even a benign re-order — must update this test
    // explicitly so the cross-host air-ground contract stays visible.
    let args = non_default_args(PathBuf::from("/etc/ados/secrets/wfb-keypair"));
    let argv = argv_for(&args);
    let expected: Vec<String> = vec![
        "-K".into(),
        "/etc/ados/secrets/wfb-keypair".into(),
        "-k".into(),
        "4".into(),
        "-n".into(),
        "8".into(),
        "-p".into(),
        "2".into(),
        "-B".into(),
        "40".into(),
        "-G".into(),
        "short".into(),
        "-M".into(),
        "4".into(),
        "-S".into(),
        "1".into(),
        "-L".into(),
        "1".into(),
        "-u".into(),
        "5601".into(),
        "wlan0".into(),
    ];
    assert_eq!(argv, expected);
}

#[test]
fn argv_with_default_advanced_opts_matches_documented_defaults() {
    let args = WfbTxArgs {
        interface: "wlan1".to_string(),
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        keypair_path: PathBuf::from(DEFAULT_KEYPAIR_PATH),
        advanced: WfbAdvancedOpts::default(),
    };
    let argv = argv_for(&args);

    // Documented WFB-ng example config defaults: fec_k=8, fec_n=12,
    // radio_port=1, bandwidth=20, guard=long, stbc=0, ldpc=0, udp=5600.
    assert_eq!(find_flag(&argv, "-K"), Some(DEFAULT_KEYPAIR_PATH));
    assert_eq!(find_flag(&argv, "-k"), Some("8"));
    assert_eq!(find_flag(&argv, "-n"), Some("12"));
    assert_eq!(find_flag(&argv, "-p"), Some("1"));
    assert_eq!(find_flag(&argv, "-B"), Some("20"));
    assert_eq!(find_flag(&argv, "-G"), Some("long"));
    assert_eq!(find_flag(&argv, "-M"), Some("1"));
    assert_eq!(find_flag(&argv, "-S"), Some("0"));
    assert_eq!(find_flag(&argv, "-L"), Some("0"));
    assert_eq!(find_flag(&argv, "-u"), Some("5600"));
    assert_eq!(argv.last().map(String::as_str), Some("wlan1"));
}

#[test]
fn argv_keypair_path_round_trips_with_spaces_and_unicode() {
    // Path with a space + a non-ASCII byte. The composer hands argv to
    // `tokio::process::Command::args`, which does NOT shell-quote; it
    // passes each arg as a separate execve argv slot. The path must
    // round-trip verbatim or the operator's choice of mount point is
    // silently corrupted.
    let exotic = PathBuf::from("/var/lib/ados/wfb keypair αβγ.bin");
    let args = WfbTxArgs {
        interface: "wlan0".to_string(),
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        keypair_path: exotic.clone(),
        advanced: WfbAdvancedOpts::default(),
    };
    let argv = argv_for(&args);
    let pos = argv.iter().position(|s| s == "-K").expect("-K must appear");
    assert_eq!(argv[pos + 1], exotic.to_string_lossy());
}

#[tokio::test]
async fn build_args_returns_none_until_dongle_bound() {
    // The manager's `build_args` is the only public path that exercises
    // both the keypair-passphrase validation and the optional `interface`
    // gate. With no dongle attached the call returns `Ok(None)` — the
    // contract a healthy supervisor relies on so it does not try to spawn
    // `wfb_tx` against a nonexistent interface.
    let cfg = WfbConfig {
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        key_passphrase: "build-args-test-passphrase".to_string(),
        wfb_tx_path: PathBuf::from(DEFAULT_WFB_TX_PATH),
        interface: None,
        keypair_path: PathBuf::from(DEFAULT_KEYPAIR_PATH),
        advanced: WfbAdvancedOpts::default(),
    };
    let mgr = WfbManager::new(cfg).expect("construct");
    assert!(
        mgr.build_args().await.expect("build_args ok").is_none(),
        "no dongle attached -> Ok(None) contract"
    );
}

#[tokio::test]
async fn build_args_with_empty_passphrase_returns_args_after_binding() {
    // An empty passphrase signals "keep the existing keypair file on
    // disk untouched" — the path operators take when retuning channel /
    // MCS / power without rotating the broadcast secret. The argv
    // composer therefore must NOT treat empty as a typed error; it
    // returns Ok(Some(_)) with the channel/MCS/power baked in and
    // wfb_tx reads the keypair bytes already on disk via -K.
    use ados_wfb::DongleEvent;
    let cfg = WfbConfig {
        channel: 161,
        mcs_index: 1,
        tx_power_dbm: 25,
        key_passphrase: String::new(),
        wfb_tx_path: PathBuf::from(DEFAULT_WFB_TX_PATH),
        interface: None,
        keypair_path: PathBuf::from(DEFAULT_KEYPAIR_PATH),
        advanced: WfbAdvancedOpts::default(),
    };
    let mgr = WfbManager::new(cfg).expect("construct");
    mgr.handle_dongle_event(DongleEvent::Added("wlan0".to_string()))
        .await;
    let args = mgr
        .build_args()
        .await
        .expect("build_args ok with empty passphrase")
        .expect("interface bound -> Some");
    assert_eq!(args.channel, 161);
    assert_eq!(args.mcs_index, 1);
    assert_eq!(args.tx_power_dbm, 25);
}
