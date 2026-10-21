//! The terminal-console showcase: renders every notable screen from one
//! coherent fixture and, when `ADOS_TUI_SHOWCASE_OUT` names a path, writes
//! the frames there as a truecolor replay document.

mod export;
mod fixtures;
mod frames;
mod pages;

/// The text each frame must show: its tab label or overlay title.
const EXPECT: [(&str, &str); 18] = [
    ("overview", "Overview"),
    ("vehicle", "Vehicle"),
    ("link", "Link"),
    ("video", "Video"),
    ("params", "ATC_RAT_RLL_P"),
    ("params-write", "Write parameters"),
    ("extensions", "INSTALLED"),
    ("logs", "Level"),
    ("system", "Services"),
    ("settings-appearance", "Appearance"),
    ("settings-wifi", "Nearby networks"),
    ("settings-mcp", "Tokens"),
    ("palette", "Command palette"),
    ("help", "Keys"),
    ("remote-locked", "LOCKED"),
    ("theme-light", "Overview"),
    ("theme-nvg", "Overview"),
    ("ground", "Ground"),
];

/// Third-party ground stations: the console names endpoints by kind.
const BANNED: [&str; 3] = ["Mission Planner", "QGC", "QGroundControl"];

/// The only IPv4 literals a frame may show.
const ALLOWED_IPS: [&str; 2] = [fixtures::LAN_IP, "127.0.0.1"];

/// Every dotted-quad literal in `text`.
fn ipv4_literals(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .flat_map(|token| {
            let parts: Vec<&str> = token.split('.').collect();
            parts
                .windows(4)
                .filter(|w| {
                    w.iter().all(|p| {
                        (1..=3).contains(&p.len()) && p.parse::<u16>().is_ok_and(|n| n < 256)
                    })
                })
                .map(|w| w.join("."))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn ipv4_scan_finds_dotted_quads_only() {
    assert_eq!(
        ipv4_literals("http://10.0.0.7:8080 v0.106.1 at 192.168.1.50."),
        vec!["10.0.0.7".to_string(), "192.168.1.50".to_string()]
    );
}

#[test]
fn showcase_frames_render_and_export() {
    assert_eq!(EXPECT.map(|(n, _)| n), frames::NAMES);
    let mut out = Vec::new();
    for (name, want) in EXPECT {
        let lines = export::to_frames(&frames::build(name));
        let text = export::text(&lines);
        assert_eq!(lines.len(), usize::from(export::HEIGHT), "{name}");
        assert!(text.contains(want), "{name}: missing {want:?}\n{text}");
        for bad in BANNED {
            assert!(!text.contains(bad), "{name}: shows {bad:?}\n{text}");
        }
        for ip in ipv4_literals(&text) {
            assert!(
                ALLOWED_IPS.contains(&ip.as_str()),
                "{name}: shows {ip}\n{text}"
            );
        }
        out.push((name, lines));
    }
    if let Some(path) = std::env::var_os("ADOS_TUI_SHOWCASE_OUT") {
        std::fs::write(&path, export::document(out)).expect("write the showcase document");
    }
}
