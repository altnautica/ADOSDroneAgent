use super::*;
use crate::pages::gs_ctx::{GnssBaseCtx, LoraBeacon, LoraDutyRule, RadioAdapterReport};
use crate::pages::reading::Reading;

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn lora(reason: Option<&str>, next_ms: u64) -> LoraCtx {
    LoraCtx {
        region_code: Some("IN".into()),
        tx_allowed: reason.is_none(),
        blocked_reason: reason.map(str::to_string),
        next_tx_allowed_ms: next_ms,
        duty_rules: vec![LoraDutyRule {
            sub_band: None,
            used_pct: 0.4,
            limit_pct: 1.0,
        }],
        erp_mw: Some(25.0),
        beacons_heard: 2,
        beacons: vec![
            LoraBeacon {
                node_id: 42,
                rssi_dbm: Some(-92.0),
                snr_db: Some(7.5),
                age_s: Some(12),
            },
            LoraBeacon {
                node_id: 7,
                rssi_dbm: Some(-101.0),
                snr_db: Some(2.0),
                age_s: Some(41),
            },
        ],
        ..LoraCtx::default()
    }
}

fn adapter(role: AdapterRole, tx: bool) -> RadioAdapterReport {
    RadioAdapterReport {
        iface: "wlan1".into(),
        mac: None,
        usb_path: None,
        driver: "rtl88x2eu".into(),
        role,
        antenna: None,
        tx_active: tx,
        rssi_dbm: vec![-48, -51],
        packets: 981,
        lost: 19,
        fec_rec: None,
        verified: true,
    }
}

fn full_ctx() -> PageContext {
    PageContext {
        gnss: Reading::Present(GnssCtx {
            fix: Some("rtk_fixed".into()),
            sats: Some(24),
            hdop: Some(0.7),
            pps_locked: Some(true),
            base: Some(GnssBaseCtx {
                mode: Some("survey".into()),
                state: Some("surveying".into()),
                elapsed_s: Some(214.0),
                min_dur_s: Some(300.0),
                accuracy_m: Some(1.4),
                target_accuracy_m: Some(2.0),
            }),
            heading_deg: Some(274.3),
        }),
        radio_adapters: Reading::Present(RadioAdaptersCtx {
            adapters: vec![
                adapter(AdapterRole::Rxtx, true),
                adapter(AdapterRole::Rx, false),
                adapter(AdapterRole::Mesh, false),
            ],
            ..RadioAdaptersCtx::default()
        }),
        lora: Reading::Present(lora(None, 0)),
        ..PageContext::default()
    }
}

#[test]
fn rotation_follows_config_order_and_skips_unknown_and_absent() {
    let geom = OledGeometry::W128_H64;
    let screens = ids(&["lora", "link", "home", "plugin:x", "gnss", "radios", "home"]);
    assert_eq!(
        rotation(&screens, &full_ctx(), geom),
        vec![
            OledScreen::Lora,
            OledScreen::Home,
            OledScreen::Gnss,
            OledScreen::Radios
        ]
    );
    let mut ctx = full_ctx();
    ctx.gnss = Reading::Absent;
    // A failed read is skipped too, never shown as a blank screen.
    ctx.lora = Reading::Unreachable;
    assert_eq!(
        rotation(&screens, &ctx, geom),
        vec![OledScreen::Home, OledScreen::Radios]
    );
}

#[test]
fn rotation_falls_back_to_home_and_32px_is_home_only() {
    let ctx = full_ctx();
    assert_eq!(
        rotation(&ids(&["qr", "system"]), &ctx, OledGeometry::W128_H64),
        vec![OledScreen::Home]
    );
    assert_eq!(
        rotation(&ids(&["gnss", "lora"]), &ctx, OledGeometry::W128_H32),
        vec![OledScreen::Home]
    );
}

#[test]
fn rotator_dwells_then_advances_and_a_single_screen_stays() {
    let list = [OledScreen::Home, OledScreen::Lora];
    let t0 = Instant::now();
    let mut r = Rotator::default();
    assert_eq!(r.current(&list, t0), OledScreen::Home);
    assert_eq!(
        r.current(&list, t0 + Duration::from_secs(4)),
        OledScreen::Home
    );
    assert_eq!(r.current(&list, t0 + DWELL), OledScreen::Lora);
    assert_eq!(r.current(&list, t0 + DWELL * 2), OledScreen::Home);
    let single = [OledScreen::Home];
    for s in 0..20 {
        assert_eq!(
            r.current(&single, t0 + Duration::from_secs(s)),
            OledScreen::Home
        );
    }
}

#[test]
fn lora_screen_lines() {
    let ctx = full_ctx();
    let lines = screen_lines(OledScreen::Lora, &ctx).unwrap();
    assert_eq!(lines[0], "LORA IN duty 0.4%");
    assert_eq!(lines[1], "TX OK  25mW");
    assert_eq!(lines[2], "heard 2  last 12s");
    assert_eq!(lines[3], "42 -92dB 7.5");
    assert_eq!(lines[4], "7 -101dB 2.0");

    let waiting = lora_lines(&lora(Some("duty_exhausted"), 462_000));
    assert_eq!(waiting[1], "TX WAIT 07:42  25mW");
    let mut unset = lora(Some("region_unset"), 0);
    unset.region_code = None;
    unset.duty_rules.clear();
    let lines = lora_lines(&unset);
    assert_eq!(lines[0], "LORA --");
    assert!(lines[1].starts_with("TX BLK REGION"));
    for l in screen_lines(OledScreen::Lora, &ctx).unwrap() {
        assert!(l.len() <= 20, "{l}");
    }
}

#[test]
fn gnss_and_radios_screen_lines() {
    let ctx = full_ctx();
    let g = screen_lines(OledScreen::Gnss, &ctx).unwrap();
    assert_eq!(
        g,
        vec![
            "GNSS  RTK-FIX  24sv",
            "base SURVEY 214s",
            "acc 1.4/2.0m",
            "PPS LOCK",
            "hdg 274.3"
        ]
    );
    let mut ctx = ctx;
    ctx.mesh.peer_count = Some(2);
    let r = screen_lines(OledScreen::Radios, &ctx).unwrap();
    assert_eq!(
        r,
        vec![
            "RADIOS 3",
            "1 RXTX -48 1.9% TX",
            "2 RX   -48 1.9%",
            "3 MESH peers 2"
        ]
    );
    for l in g.iter().chain(&r) {
        assert!(l.len() <= 20, "{l}");
    }
    assert_eq!(screen_lines(OledScreen::Home, &ctx), None);
    assert_eq!(
        screen_lines(OledScreen::Gnss, &PageContext::default()),
        None
    );
}

#[test]
fn service_screens_paint_five_lines_inside_the_panel() {
    let geom = OledGeometry::W128_H64;
    let lines = screen_lines(OledScreen::Lora, &full_ctx()).unwrap();
    let canvas = render_lines(&lines, geom, &screen_font());
    assert_eq!(canvas.len(), 128 * 64);
    // The fifth line starts at row 48 (the 64 / 5 pitch) and inks below it.
    assert!((48..64).any(|y| canvas[y * 128..(y + 1) * 128].contains(&1)));
    // 20 columns fit across the 128 px panel.
    let (w, _) = screen_font().text_size("MMMMMMMMMMMMMMMMMMMM");
    assert!(w <= 128, "20 columns are {w} px");
}
