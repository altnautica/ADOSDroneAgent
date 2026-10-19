//! Default BOM peripheral manifests seeded into `/etc/ados/peripherals/`.

use std::path::Path;

use super::set_mode;

/// Peripheral seed manifests dir under /etc/ados.
const PERIPHERALS_DIR: &str = "/etc/ados/peripherals";

/// Seed the default BOM peripheral manifests at `/etc/ados/peripherals/` so the
/// webapp Peripherals page renders the FC, GPS, radio, OLED, SPI LCD, and camera
/// entries on a fresh board. Ports 09-config.sh `seed_default_peripherals`:
/// copy every `*.yaml` from `<source>/scripts/peripherals-seed/`, but PRESERVE
/// operator-edited files — a target is only overwritten when it is clearly one of
/// our shipped seeds (`id: ados.` on the first matching line) and the bytes
/// differ. Operator-added manifests (any non-`ados.` id) are never trampled.
pub fn seed_default_peripherals(source: Option<&Path>) {
    let _ = std::fs::create_dir_all(PERIPHERALS_DIR);
    set_mode(Path::new(PERIPHERALS_DIR), 0o755);
    let src_dir = match source.map(|s| s.join("scripts/peripherals-seed")) {
        Some(p) if p.is_dir() => p,
        _ => {
            tracing::warn!("peripherals-seed source dir not found; skipping seed manifests");
            return;
        }
    };
    let read = match std::fs::read_dir(&src_dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut copied = 0usize;
    let mut refreshed = 0usize;
    for entry in read.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.ends_with(".yaml") => n.to_string(),
            _ => continue,
        };
        let target = Path::new(PERIPHERALS_DIR).join(&name);
        let seed_bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if !target.exists() {
            if std::fs::write(&target, &seed_bytes).is_ok() {
                set_mode(&target, 0o644);
                copied += 1;
            }
        } else if target_is_ados_seed(&target) {
            let existing = std::fs::read(&target).unwrap_or_default();
            if existing != seed_bytes && std::fs::write(&target, &seed_bytes).is_ok() {
                set_mode(&target, 0o644);
                refreshed += 1;
            }
        }
    }
    tracing::info!(copied, refreshed, "default peripheral manifests seeded");
}

/// True when a target manifest declares an `id: ados.` (our shipped seed) on a
/// non-comment line — the overwrite gate that preserves operator-added files.
/// Mirrors the bash `grep -q '^id:[[:space:]]*ados\.'`.
fn target_is_ados_seed(target: &Path) -> bool {
    let body = match std::fs::read_to_string(target) {
        Ok(b) => b,
        Err(_) => return false,
    };
    first_line_declares_ados_id(&body)
}

/// Pure: does any line declare `id: ados.<...>` (leading whitespace then `id:`,
/// optional whitespace, then the `ados.` prefix)? Mirrors the bash grep.
pub fn first_line_declares_ados_id(body: &str) -> bool {
    for raw in body.lines() {
        let line = raw.trim_start();
        if let Some(rest) = line.strip_prefix("id:") {
            return rest.trim_start().starts_with("ados.");
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ados_seed_id_is_detected_and_operator_files_are_preserved() {
        // A shipped seed (id: ados.<...>) is overwriteable.
        assert!(first_line_declares_ados_id(
            "name: Flight Controller\nid: ados.flight-controller\n"
        ));
        assert!(first_line_declares_ados_id("id:ados.gps\n"));
        assert!(first_line_declares_ados_id("id:   ados.radio\n"));
        // An operator-added manifest (non-ados id, or no id) is preserved.
        assert!(!first_line_declares_ados_id("id: custom.thing\n"));
        assert!(!first_line_declares_ados_id("name: Thing\nvendor: acme\n"));
        assert!(!first_line_declares_ados_id(""));
        // A commented-out id line is not a real declaration.
        assert!(!first_line_declares_ados_id(
            "# id: ados.commented\nid: custom.real\n"
        ));
    }
}
