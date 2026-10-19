//! Render every LCD page to a PNG for visual parity review.
//!
//! Builds two fully-populated [`PageContext`] fixtures (a paired, link-up
//! ground station with live drone / mesh / uplink data, and a fresh-boot
//! unpaired variant), paints every registered page, and writes the 480x320
//! frame to `target/ui-parity/<page>_<variant>.png`. A third fixture, the mesh
//! receiver, adds `mesh_receiver.png` and `mesh_role_picker.png` (the role
//! picker opened with RELAY armed, reading "Tap again: RELAY").
//!
//! Run on the host toolchain (the framebuffer write path is not exercised here,
//! only the pure page composers):
//!
//! ```text
//! cargo run -p ados-display --example render_pages
//! ```

mod fixtures;

use std::path::{Path, PathBuf};

use ados_display::graphics::palette::{Palette, DARK};
use ados_display::graphics::primitives::Canvas;
use ados_display::pages::{
    about::AboutDetailPage, channel_hops::ChannelHopsPage, dashboard::DashboardPage,
    diagnostics::DiagnosticsDetailPage, drone::DroneDetailPage, link_stats::LinkStatsPage,
    mesh::MeshDetailPage, more::MorePage, pair_drone::PairDroneDetailPage,
    radio_link::RadioLinkDetailPage, settings::SettingsPage, uplink::UplinkDetailPage,
    video::VideoPage, Page, PageContext, PanelAction, GS_TAB_IDS,
};
use ados_display::widgets::draw_bottom_bar;

use fixtures::{connected_context, receiver_context, unpaired_context};

/// One renderable page paired with the stable id used in the output filename.
struct PageEntry {
    file_id: &'static str,
    page: Box<dyn Page>,
}

/// Every page the LCD UI ships, in the same registration order the daemon uses.
fn all_pages() -> Vec<PageEntry> {
    vec![
        PageEntry {
            file_id: "dashboard",
            page: Box::new(DashboardPage),
        },
        PageEntry {
            file_id: "video",
            page: Box::new(VideoPage),
        },
        PageEntry {
            file_id: "settings",
            page: Box::new(SettingsPage),
        },
        PageEntry {
            file_id: "link_stats",
            page: Box::new(LinkStatsPage),
        },
        PageEntry {
            file_id: "channel_hops",
            page: Box::new(ChannelHopsPage),
        },
        PageEntry {
            file_id: "more",
            page: Box::new(MorePage::default()),
        },
        PageEntry {
            file_id: "radio_link",
            page: Box::new(RadioLinkDetailPage::new()),
        },
        PageEntry {
            file_id: "uplink",
            page: Box::new(UplinkDetailPage),
        },
        PageEntry {
            file_id: "drone",
            page: Box::new(DroneDetailPage),
        },
        PageEntry {
            file_id: "mesh",
            page: Box::new(MeshDetailPage::new()),
        },
        PageEntry {
            file_id: "about",
            page: Box::new(AboutDetailPage),
        },
        PageEntry {
            file_id: "pair_drone",
            page: Box::new(PairDroneDetailPage::default()),
        },
        PageEntry {
            file_id: "diagnostics",
            page: Box::new(DiagnosticsDetailPage),
        },
    ]
}

/// Check a finished page canvas is the panel's native 480x320 and write it to
/// `out_dir/<name>` as an RGB PNG (the canvas holds tightly-packed RGB888).
fn save(
    canvas: &Canvas,
    out_dir: &Path,
    name: &str,
    written: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        canvas.width(),
        ados_display::pages::PANEL_W,
        "{name} rendered the wrong width"
    );
    assert_eq!(
        canvas.height(),
        ados_display::pages::PANEL_H,
        "{name} rendered the wrong height"
    );
    let buffer: image::RgbImage =
        image::ImageBuffer::from_raw(canvas.width(), canvas.height(), canvas.as_rgb888().to_vec())
            .ok_or("canvas RGB888 buffer did not match its declared geometry")?;
    let path = out_dir.join(name);
    buffer.save(&path)?;
    written.push(path.display().to_string());
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent (the crates workspace dir)")
        .join("target")
        .join("ui-parity");
    std::fs::create_dir_all(&out_dir)?;

    let palette: &Palette = &DARK;
    let variants: [(&str, PageContext); 2] = [
        ("connected", connected_context()),
        ("unpaired", unpaired_context()),
    ];

    let pages = all_pages();
    let mut written: Vec<String> = Vec::new();

    for (variant, ctx) in &variants {
        for entry in &pages {
            let mut canvas = entry.page.render(ctx, palette);
            // The navigator paints the tab bar over a tabbed page on the panel.
            if entry.page.chrome().has_tab_bar() {
                draw_bottom_bar(&mut canvas, palette, &GS_TAB_IDS, entry.page.id());
            }
            let name = format!("{}_{variant}.png", entry.file_id);
            save(&canvas, &out_dir, &name, &mut written)?;
        }
    }

    // The mesh receiver: its peer list, then the role picker with RELAY armed
    // (first tap only; the confirming second tap would send the role change).
    let receiver = receiver_context();
    let mesh = MeshDetailPage::new();
    save(
        &mesh.render(&receiver, palette),
        &out_dir,
        "mesh_receiver.png",
        &mut written,
    )?;
    let picker = MeshDetailPage::new();
    for key in ["mesh.switch_role", "mesh.role.relay"] {
        assert!(
            matches!(picker.on_custom(key, &receiver), Some(PanelAction::Repaint)),
            "{key} should only repaint the picker"
        );
    }
    save(
        &picker.render(&receiver, palette),
        &out_dir,
        "mesh_role_picker.png",
        &mut written,
    )?;

    println!(
        "rendered {} page frames to {}",
        written.len(),
        out_dir.display()
    );
    for p in &written {
        println!("  {p}");
    }
    Ok(())
}
