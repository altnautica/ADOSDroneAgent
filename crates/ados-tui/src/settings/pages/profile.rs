//! Settings page: IDENTITY › Profile. Shows the detected profile, its source
//! and the ground role; ←/→ stages a new profile or role and Enter applies it
//! (after a confirm naming what the change stops) through
//! `POST /api/v1/setup/apply {profile: {profile, ground_role?, auto_restart}}`.
//! `d` re-runs hardware detection.

use ratatui::layout::Rect;
use ratatui::Frame;
use serde_json::{json, Value};

use super::kit;
use crate::app::{Core, FrameOut, Hit};
use crate::keymap::{Command, PageKeys};
use crate::overlay::{Overlay, OverlayResult, Ret};
use crate::poll::{ResourceId, Sub};
use crate::settings::apply_result;
use crate::settings::page::{Bespoke, BespokeId};
use crate::settings::row::cycle;
use crate::settings::rows_view::{self, Item, RowsState};
use crate::toast::ToastKind;
use crate::worker::{Purpose, Reply, Request, Route};

const PROFILES: &[&str] = &["drone", "ground_station", "visor"];
const ROLES: &[&str] = &["direct", "relay", "receiver"];

const PROFILE_HELP: &str =
    "Drone: air-side companion. Ground station: ground-side receiver. Visor: handheld capture rig.";
const ROLE_HELP: &str = "Direct: single ground node. Relay: forwards a drone link to another ground node. Receiver: aggregates streams from relay nodes.";

fn route() -> Route {
    Route::page(BespokeId::Profile.id())
}

#[derive(Default)]
pub struct Page {
    rows: RowsState,
    staged_profile: Option<String>,
    staged_role: Option<String>,
    busy: bool,
}

/// The applied profile and role from the setup status.
fn current(core: &Core) -> (String, String) {
    let s = kit::setup(core);
    let profile = kit::text(s, "profile")
        .unwrap_or_else(|| core.profile())
        .to_string();
    let role = kit::text(s, "ground_role").unwrap_or("direct").to_string();
    (profile, role)
}

fn profile_label(p: &str) -> &str {
    match p {
        "drone" => "Drone",
        "ground_station" => "Ground station",
        "visor" => "Visor",
        other => other,
    }
}

/// What applying `target` on a node now running `current` does to its
/// services, the same lines the web settings confirm with. A profile change
/// moves the node onto the new profile's services; the HDMI kiosk goes with a
/// ground station and the access point with a ground station or visor that
/// becomes a drone. A role change is a plain restart.
fn change_notes(current: &str, target: &str) -> Vec<&'static str> {
    if current == target {
        return vec![
            "ados-supervisor restarts to apply the role. This session may disconnect for a few seconds.",
        ];
    }
    let mut notes = vec![
        "The node moves its services to the new profile: it stops and disables the old profile's services and restarts. This session may disconnect until the agent is back.",
    ];
    if current == "ground_station" {
        notes.push("Leaving the ground-station profile turns off its HDMI kiosk screen.");
    }
    if current != "drone" && target == "drone" {
        notes.push(
            "A drone runs no Wi-Fi access point, so the node's access point stops. A client joined to it loses the node; reach it over another network.",
        );
    }
    notes
}

impl Page {
    fn effective(&self, core: &Core) -> (String, String) {
        let (p, r) = current(core);
        (
            self.staged_profile.clone().unwrap_or(p),
            self.staged_role.clone().unwrap_or(r),
        )
    }

    fn dirty(&self, core: &Core) -> bool {
        let (cur_p, cur_r) = current(core);
        let (p, r) = self.effective(core);
        p != cur_p || (p == "ground_station" && r != cur_r)
    }

    fn items(&self, core: &Core) -> Vec<Item> {
        let s = kit::setup(core);
        let (cur_p, cur_r) = current(core);
        let (p, r) = self.effective(core);
        let mut items = vec![Item::Header("Detected".into())];
        if let Some(note) = kit::resource_note(core, &ResourceId::SetupStatus, "the setup status") {
            items.push(note);
        }
        let role_text = if cur_p == "ground_station" {
            format!(" / role {cur_r}")
        } else {
            String::new()
        };
        items.push(rows_view::info(
            core,
            "Current",
            format!("{}{role_text}", profile_label(&cur_p)),
        ));
        items.push(rows_view::info(
            core,
            "Source",
            kit::text(s, "profile_source").unwrap_or("unknown"),
        ));
        if let Some(d) = kit::text(s, "profile_suggestion.detected") {
            let hint = kit::text(s, "profile_suggestion.ground_role_hint").unwrap_or("direct");
            let role = if d == "ground_station" {
                format!(" / role {hint}")
            } else {
                String::new()
            };
            items.push(rows_view::info(
                core,
                "Hardware suggests",
                format!("{}{role}", profile_label(d)),
            ));
        }

        items.push(Item::Header("Profile".into()));
        let mut row = kit::row(
            "profile",
            "Profile",
            PROFILE_HELP,
            kit::enum_of(PROFILES),
            Some(json!(p)),
        );
        row.default = Some(json!(cur_p));
        items.push(Item::Row(row));
        if p == "ground_station" {
            let mut row = kit::row(
                "ground_role",
                "Ground station role",
                ROLE_HELP,
                kit::enum_of(ROLES),
                Some(json!(r)),
            );
            row.default = Some(json!(if cur_p == "ground_station" {
                cur_r
            } else {
                String::new()
            }));
            items.push(Item::Row(row));
        }
        let detail = if self.busy {
            format!("{} Applying…", core.sym.spinner_frame(core.frame as usize))
        } else if self.dirty(core) {
            "unsaved changes; applying restarts the agent's services".into()
        } else {
            "no changes".into()
        };
        items.push(kit::action(core, "apply", "Apply", &detail));
        items.push(kit::action(core, "redetect", "Re-detect hardware", "d"));
        items
    }

    fn stage(&mut self, core: &Core, forward: bool) {
        let items = self.items(core);
        let Some(row) = self.rows.focused_row(&items) else {
            return;
        };
        let Some(next) = cycle(&row.kind, row.value.as_ref(), forward) else {
            return;
        };
        let next = next.as_str().unwrap_or_default().to_string();
        let (cur_p, cur_r) = current(core);
        match row.key.as_str() {
            "profile" => self.staged_profile = (next != cur_p).then_some(next),
            "ground_role" => self.staged_role = (next != cur_r).then_some(next),
            _ => {}
        }
    }

    fn apply(&mut self, core: &mut Core) {
        if !self.dirty(core) {
            core.toast(ToastKind::Info, "No profile change to apply");
            return;
        }
        if !core.guard_disarmed() {
            return;
        }
        let (p, r) = self.effective(core);
        let (cur_p, _) = current(core);
        let first = if p == "ground_station" {
            format!("The agent will reconfigure for {p} role {r}.")
        } else {
            format!("The agent will reconfigure for {p}.")
        };
        let mut lines = vec![first];
        lines.extend(change_notes(&cur_p, &p).into_iter().map(String::from));
        core.open(Overlay::confirm(
            Ret::new(route(), "apply").with(json!({ "profile": p, "role": r })),
            "Switch agent profile?",
            lines,
        ));
    }

    fn redetect(&mut self, core: &mut Core) {
        core.submit(Request::new(
            "POST",
            "/api/v1/setup/profile/redetect",
            route(),
            Purpose::Generic {
                ok_toast: "Hardware detection refreshed.".into(),
            },
        ));
    }
}

impl Bespoke for Page {
    fn subs(&self, _core: &Core) -> Vec<Sub> {
        vec![kit::setup_sub()]
    }

    fn keys(&self) -> Option<PageKeys> {
        Some(PageKeys::Profile)
    }

    fn on_command(&mut self, core: &mut Core, cmd: Command) -> bool {
        let items = self.items(core);
        if kit::nav(&mut self.rows, &items, cmd) {
            return true;
        }
        match cmd {
            Command::Left | Command::Right => self.stage(core, cmd == Command::Right),
            Command::Redetect => self.redetect(core),
            Command::Activate => match self.rows.focused_key(&items) {
                Some("redetect") => self.redetect(core),
                _ => self.apply(core),
            },
            _ => return false,
        }
        true
    }

    fn on_overlay(&mut self, core: &mut Core, ret: Ret, result: OverlayResult) {
        if ret.tag != "apply" || result != OverlayResult::Confirmed {
            return;
        }
        // Re-check: the vehicle may have armed while the dialog was open.
        if !core.guard_disarmed() {
            return;
        }
        let profile = ret.data.get("profile").cloned().unwrap_or(Value::Null);
        let mut section = json!({ "profile": profile, "auto_restart": true });
        if profile == json!("ground_station") {
            section["ground_role"] = ret.data.get("role").cloned().unwrap_or(Value::Null);
        }
        self.busy = true;
        core.submit(
            Request::new(
                "POST",
                "/api/v1/setup/apply",
                route(),
                Purpose::SetupApply { section: "profile" },
            )
            .body(json!({ "profile": section })),
        );
    }

    fn on_reply(&mut self, core: &mut Core, reply: Reply) {
        match reply.purpose {
            Purpose::SetupApply { .. } => {
                self.busy = false;
                if let Some(body) = apply_result::show(core, "Profile", &reply.result) {
                    if apply_result::succeeded(body, "profile") {
                        self.staged_profile = None;
                        self.staged_role = None;
                        core.flash("profile:profile", true);
                    } else {
                        core.flash("profile:profile", false);
                    }
                }
                core.fetch_now(ResourceId::SetupStatus);
            }
            Purpose::Generic { .. } => core.fetch_now(ResourceId::SetupStatus),
            _ => {}
        }
    }

    fn on_click(&mut self, core: &mut Core, hit: &Hit) {
        let items = self.items(core);
        kit::click(&mut self.rows, &items, hit);
    }

    fn render(&mut self, f: &mut Frame, area: Rect, core: &Core, out: &mut FrameOut) {
        let items = self.items(core);
        rows_view::render(f, area, core, &items, &mut self.rows, "profile:", true, out);
    }

    fn context_line(&self, core: &Core) -> Option<String> {
        let items = self.items(core);
        match self.rows.focused_key(&items)? {
            "apply" => Some(
                "Apply the staged profile; the node stops the old profile's services and starts the new"
                    .into(),
            ),
            "redetect" => Some("Re-run the hardware probes and refresh the suggestion".into()),
            _ => self.rows.focused_row(&items).map(|r| r.help.clone()),
        }
    }

    fn animating(&self, _core: &Core) -> bool {
        self.busy
    }
}

#[cfg(test)]
mod tests {
    use super::change_notes;

    #[test]
    fn the_confirm_names_what_a_switch_stops() {
        assert_eq!(change_notes("ground_station", "ground_station").len(), 1);
        assert!(change_notes("ground_station", "ground_station")[0].contains("role"));
        let gs_to_drone = change_notes("ground_station", "drone");
        assert_eq!(gs_to_drone.len(), 3);
        assert!(gs_to_drone[1].contains("HDMI kiosk"));
        assert!(gs_to_drone[2].contains("access point"));
        // A visor keeps the access point; a visor that becomes a drone does not.
        assert_eq!(change_notes("ground_station", "visor").len(), 2);
        assert_eq!(change_notes("visor", "drone").len(), 2);
        assert_eq!(change_notes("drone", "ground_station").len(), 1);
    }
}
