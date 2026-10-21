//! The arm-state guard and the auth-prompt routing.

use std::time::Duration;

use serde_json::{json, Value};

use super::ARMED_REFUSAL;
use crate::model::ProfileRoutes;
use crate::overlay::{AuthMode, Overlay};
use crate::ui::tests::{feed, test_app};

fn drone(armed: Value) -> Value {
    json!({
        "profile": "drone", "mavlink": {"connected": true},
        "telemetry": {"armed": armed, "mode": "STABILIZE"}
    })
}

#[test]
fn only_a_fresh_disarmed_reading_or_no_autopilot_passes_the_guard() {
    let check = |setup: Value, age: u64| {
        let mut app = test_app();
        feed(&mut app, setup, ProfileRoutes::default(), Duration::from_secs(age));
        app.core.require_disarmed()
    };
    assert_eq!(check(drone(json!(false)), 0), Ok(()));
    assert_eq!(check(drone(json!(true)), 0), Err(ARMED_REFUSAL));
    assert_eq!(check(drone(Value::Null), 0), Err(ARMED_REFUSAL), "unknown arm state");
    assert_eq!(check(drone(json!(false)), 30), Err(ARMED_REFUSAL), "stale reading");
    // A drone whose FC link is down may be a vehicle in the air.
    assert_eq!(
        check(json!({"profile": "drone", "mavlink": {"connected": false}}), 0),
        Err(ARMED_REFUSAL)
    );
    // Nodes with no autopilot have nothing to be armed.
    assert_eq!(check(json!({"profile": "ground_station"}), 0), Ok(()));
    assert_eq!(check(json!({"profile": "visor", "mavlink": {"connected": false}}), 0), Ok(()));
    assert_eq!(check(json!({"profile": "ground_station"}), 30), Err(ARMED_REFUSAL));
    // Before the first poll nothing is known.
    assert_eq!(test_app().core.require_disarmed(), Err(ARMED_REFUSAL));
}

fn auth_prompt(app: &crate::app::App) -> Option<AuthMode> {
    app.core.overlays.iter().find_map(|o| match o {
        Overlay::Auth(a) => Some(a.mode),
        _ => None,
    })
}

#[test]
fn a_401_asks_for_the_key_and_an_unrelated_403_asks_for_nothing() {
    let mut app = test_app();
    app.core.auth.paired = Some(true);
    // A write refused for its own reasons (an extension lifecycle refusal)
    // is not a credential problem.
    super::auth::on_denied(&mut app.core, 403, false);
    assert_eq!(auth_prompt(&app), None);
    super::auth::on_denied(&mut app.core, 403, true);
    assert_eq!(auth_prompt(&app), None, "a paired agent's 403 is not a PIN prompt");
    super::auth::on_denied(&mut app.core, 401, true);
    assert_eq!(auth_prompt(&app), Some(AuthMode::Key));
}

#[test]
fn a_write_refusal_never_drops_a_working_pin_session() {
    let mut app = test_app();
    app.core.auth.paired = Some(false);
    app.core.client.set_session(Some("s".into()));
    super::auth::on_denied(&mut app.core, 403, false);
    assert_eq!(app.core.client.auth().session.as_deref(), Some("s"));
    assert_eq!(auth_prompt(&app), None);
    // A read refused on an unpaired agent means the session is gone.
    super::auth::on_denied(&mut app.core, 403, true);
    assert_eq!(app.core.client.auth().session, None);
}
