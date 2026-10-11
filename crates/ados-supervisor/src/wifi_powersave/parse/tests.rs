use super::*;

#[test]
fn parses_only_station_interfaces_whatever_their_name() {
    let dev = "\
phy#0
\tInterface wlan0
\t\tifindex 3
\t\ttype managed
phy#3
\tInterface wlan1
\t\ttype monitor
phy#4
\tInterface wlx00c0ca123456
\t\ttype managed
phy#0
\tInterface p2p0
\t\ttype P2P-device
";
    assert_eq!(
        parse_station_interfaces(dev),
        vec!["wlan0", "wlx00c0ca123456"]
    );
}

#[test]
fn parses_power_save_on_off_and_unknown() {
    assert_eq!(parse_power_save("Power save: on\n"), Some(true));
    assert_eq!(parse_power_save("\tPower save: off\n"), Some(false));
    // A mixed-case reading is still parsed.
    assert_eq!(parse_power_save("power save: ON"), Some(true));
    // No power-save line at all.
    assert_eq!(parse_power_save("Interface wlan0\n\ttype managed\n"), None);
}

#[test]
fn parses_link_connected_with_signal() {
    let link = "\
Connected to 11:22:33:44:55:66 (on wlan0)
\tSSID: ExampleNet
\tfreq: 5745
\tsignal: -52 dBm
\ttx bitrate: 300.0 MBit/s
";
    let info = parse_link(link);
    assert_eq!(info.link_state, "connected");
    assert_eq!(info.signal_dbm, Some(-52));
}

#[test]
fn parses_link_not_connected() {
    let info = parse_link("Not connected.\n");
    assert_eq!(info.link_state, "disconnected");
    assert_eq!(info.signal_dbm, None);
}

#[test]
fn unparseable_link_reads_unknown_with_no_signal() {
    let info = parse_link("some unexpected output\n");
    assert_eq!(info.link_state, "unknown");
    assert_eq!(info.signal_dbm, None);
}
