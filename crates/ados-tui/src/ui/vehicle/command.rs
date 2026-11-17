//! The `POST /api/command` requests the Vehicle tab sends: a mission stall's
//! answer, a place-class change and "I rebalanced it".

use serde_json::json;

use crate::app::TabId;
use crate::model::mission_stall::StallAction;
use crate::model::route_check::{place_command, place_label};
use crate::model::spec_text::spec_text;
use crate::worker::{Purpose, Request, Route};

/// The command route a stall or place action goes out on.
const COMMAND_PATH: &str = "/api/command";
/// The reply tag of a stall action.
pub(super) const STALL_TAG: &str = "stall";
/// The reply tag of a place-class change.
pub(super) const PLACE_TAG: &str = "place";
/// The reply tag of "I rebalanced it".
pub(super) const AIRFRAME_TAG: &str = "airframe";
/// The `/api/command` name of the pilot's `airframe_reset`.
const AIRFRAME_RESET_COMMAND: &str = "airframereset";

/// `POST /api/command` with `cmd`, its words carried for the reply's toast.
fn command_request(tag: &'static str, cmd: &str, label: String) -> Request {
    Request::new(
        "POST",
        COMMAND_PATH,
        Route::tab(TabId::Vehicle),
        Purpose::Page {
            tag,
            data: json!(label),
        },
    )
    .body(json!({ "cmd": cmd }))
}

/// The request a stall action sends: the command that reaches the pilot
/// action.
pub(crate) fn stall_request(action: StallAction) -> Request {
    command_request(STALL_TAG, action.command(), action.label())
}

/// The request that sets the place class (`placeclass_<class>`, the pilot's
/// `place_class:<class>`).
pub(crate) fn place_request(class: &str) -> Request {
    command_request(PLACE_TAG, &place_command(class), place_label(class))
}

/// The request "I rebalanced it" sends: the pilot's `airframe_reset`.
pub(crate) fn airframe_reset_request() -> Request {
    command_request(
        AIRFRAME_TAG,
        AIRFRAME_RESET_COMMAND,
        spec_text("readiness.airframe_balance.reset", &[]),
    )
}
