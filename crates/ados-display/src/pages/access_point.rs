//! Access-point detail — how to join this ground station's own WiFi.
//!
//! The passphrase is generated per unit on first boot, so there is no printed
//! default and no shared secret an operator can be told once. Before this page
//! existed it was shown at install time and by the terminal status view, which
//! is no help at all to the person standing in a field holding a panel and a
//! phone: the ground station is the thing they cannot reach, so the surface
//! that tells them how to reach it must not itself require reaching it.
//!
//! The passphrase is read from its 0600 file on this box, NEVER from the polled
//! REST response — those routes answer the LAN as well as loopback, so putting
//! it in a payload would publish it to anyone who can already reach the ground
//! station, which is precisely the population this AP exists to admit
//! deliberately. A physical panel requires physical presence, which is the one
//! place showing a passphrase is appropriate.

use crate::graphics::fonts::{FontFace, LoadedFont};
use crate::graphics::palette::Palette;
use crate::graphics::primitives::{text, Canvas};
use crate::graphics::qr::render_qr;
use crate::pages::{blank_panel, HitAction, HitZone, NetworkCtx, Page, PageContext, PANEL_W};
use crate::widgets::{draw_detail_header, DETAIL_HEADER_H};

/// A WiFi join string, the format every phone camera understands.
///
/// Escaping is not cosmetic: `;` and `:` are the format's own separators, so an
/// unescaped one in a passphrase silently truncates it and the operator gets a
/// QR that scans cleanly and joins nothing. The generated alphabet avoids these
/// characters, but a configured passphrase does not have to.
pub fn wifi_join_string(ssid: &str, passphrase: Option<&str>) -> String {
    fn esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if matches!(c, '\\' | ';' | ',' | ':' | '"') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
    match passphrase {
        Some(p) if !p.is_empty() => {
            format!("WIFI:T:WPA;S:{};P:{};;", esc(ssid), esc(p))
        }
        // An open network is a different join string, not a WPA one with an
        // empty password — a phone handed the latter prompts for a passphrase
        // that does not exist.
        _ => format!("WIFI:T:nopass;S:{};;", esc(ssid)),
    }
}

/// What the page shows for the passphrase, given what it could read.
///
/// An unreadable file must never render as blank: a blank where a passphrase
/// belongs reads as "this network has no password" and sends the operator
/// looking for an open network that is not there.
pub fn passphrase_display(network: &NetworkCtx) -> (&'static str, String) {
    match network.ap_passphrase.as_deref() {
        Some(p) if !p.is_empty() => ("Password", p.to_string()),
        _ => ("Password", "unavailable".to_string()),
    }
}

/// The SSID to show, preferring the AP's own over the configured hotspot name.
pub fn ssid_display(network: &NetworkCtx) -> String {
    network
        .ap_ssid
        .clone()
        .or_else(|| network.hotspot_ssid.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unavailable".to_string())
}

pub struct AccessPointDetailPage;

impl Page for AccessPointDetailPage {
    fn id(&self) -> &'static str {
        "details.access_point"
    }

    fn refresh_hz(&self) -> f32 {
        // The passphrase does not change while somebody is reading it, and a
        // fast repaint on a slow SPI panel costs more than it buys.
        0.5
    }

    fn render(&self, ctx: &PageContext, palette: &Palette) -> Canvas {
        let mut canvas = blank_panel(palette);
        draw_detail_header(&mut canvas, palette, "Access point");

        let ssid = ssid_display(&ctx.network);
        let (pw_label, pw_value) = passphrase_display(&ctx.network);

        let label_font = LoadedFont::new(FontFace::SansBold, 11);
        let value_font = LoadedFont::new(FontFace::MonoBold, 16);

        let mut y = DETAIL_HEADER_H + 10;
        text(
            &mut canvas,
            &label_font,
            "Network",
            16,
            y,
            palette.text_secondary,
        );
        y += 14;
        text(&mut canvas, &value_font, &ssid, 16, y, palette.text_primary);

        y += 26;
        text(
            &mut canvas,
            &label_font,
            pw_label,
            16,
            y,
            palette.text_secondary,
        );
        y += 14;
        text(
            &mut canvas,
            &value_font,
            &pw_value,
            16,
            y,
            palette.text_primary,
        );

        // State the truth about whether the AP is actually up. A passphrase for
        // a network that is not broadcasting is a five-minute detour.
        y += 26;
        let (state, colour) = if ctx.network.hotspot_enabled {
            ("broadcasting", palette.text_primary)
        } else {
            ("not broadcasting", palette.text_secondary)
        };
        text(&mut canvas, &label_font, state, 16, y, colour);

        // The QR carries the same pair, so a phone joins without transcribing a
        // passphrase that was deliberately built to be unambiguous but is still
        // twelve characters read off a small panel.
        let payload = wifi_join_string(&ssid, ctx.network.ap_passphrase.as_deref());
        if let Some(qr) = render_qr(&payload, 100, 2) {
            let qr_x = PANEL_W as i32 - qr.size as i32 - 24;
            let qr_y = DETAIL_HEADER_H + 8;
            for py in 0..qr.size {
                for px in 0..qr.size {
                    if qr.is_dark(px, py) {
                        canvas.put_pixel(qr_x + px as i32, qr_y + py as i32, palette.text_primary);
                    }
                }
            }
            let hint = "scan to join";
            let hw = label_font.text_advance(hint);
            text(
                &mut canvas,
                &label_font,
                hint,
                qr_x + (qr.size as i32 - hw as i32) / 2,
                qr_y + qr.size as i32 + 6,
                palette.text_secondary,
            );
        }

        canvas
    }

    fn hit_zones(&self, _ctx: &PageContext) -> Vec<HitZone> {
        vec![HitZone::new(8, 8, 40, 32, HitAction::Back)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ssid: Option<&str>, pw: Option<&str>) -> NetworkCtx {
        NetworkCtx {
            ap_ssid: ssid.map(str::to_string),
            ap_passphrase: pw.map(str::to_string),
            hotspot_enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_wpa_join_string_carries_both_halves() {
        let s = wifi_join_string("ADOS-GS-40bb", Some("EXAMPLEPASS99"));
        assert!(s.starts_with("WIFI:T:WPA;"));
        assert!(s.contains("S:ADOS-GS-40bb;"));
        assert!(s.contains("P:EXAMPLEPASS99;"));
        assert!(s.ends_with(";;"));
    }

    #[test]
    fn separators_inside_a_value_are_escaped_not_left_to_truncate() {
        // `;` and `:` are the format's own separators. An unescaped one gives a
        // QR that scans perfectly and joins nothing, which is worse than one
        // that fails to scan — the operator blames the network, not the code.
        let s = wifi_join_string("my;net", Some("pa:ss;word"));
        assert!(s.contains("S:my\\;net;"));
        assert!(s.contains("P:pa\\:ss\\;word;"));
    }

    #[test]
    fn no_passphrase_is_an_open_network_not_wpa_with_an_empty_password() {
        // A phone handed WPA-with-empty-password prompts for a passphrase that
        // does not exist.
        for pw in [None, Some("")] {
            let s = wifi_join_string("open-net", pw);
            assert!(s.starts_with("WIFI:T:nopass;"), "{s}");
            assert!(!s.contains("P:;"), "{s}");
        }
    }

    #[test]
    fn an_unreadable_passphrase_says_so_rather_than_rendering_blank() {
        // A blank where a passphrase belongs reads as "no password needed".
        let (_, shown) = passphrase_display(&net(Some("ADOS-GS"), None));
        assert_eq!(shown, "unavailable");

        let (_, shown) = passphrase_display(&net(Some("ADOS-GS"), Some("")));
        assert_eq!(shown, "unavailable");
    }

    #[test]
    fn a_readable_passphrase_is_shown_verbatim() {
        let (_, shown) = passphrase_display(&net(Some("ADOS-GS"), Some("EXAMPLEPASS99")));
        assert_eq!(shown, "EXAMPLEPASS99");
    }

    #[test]
    fn the_ssid_falls_back_to_the_configured_hotspot_name() {
        let mut n = net(None, Some("pw"));
        n.hotspot_ssid = Some("ADOS-GS-fallback".into());
        assert_eq!(ssid_display(&n), "ADOS-GS-fallback");
    }

    #[test]
    fn a_missing_ssid_says_unavailable_rather_than_showing_an_empty_name() {
        assert_eq!(ssid_display(&NetworkCtx::default()), "unavailable");
    }
}
