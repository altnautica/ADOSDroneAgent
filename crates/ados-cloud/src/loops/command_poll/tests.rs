use super::*;
use crate::dispatch::CommandResult;

#[test]
fn ack_carries_command_id_device_status_result_and_data() {
    let result =
        CommandResult::completed("installed").with_data(serde_json::json!({"pluginId": "p"}));
    let ack = build_ack("cmd-1", "dev1", &result);
    assert_eq!(ack["commandId"], "cmd-1");
    assert_eq!(ack["deviceId"], "dev1");
    assert_eq!(ack["status"], "completed");
    assert_eq!(ack["result"]["success"], true);
    assert_eq!(ack["data"]["pluginId"], "p");
}

#[test]
fn ack_omits_data_when_absent() {
    let result = CommandResult::failed("boom");
    let ack = build_ack("cmd-2", "dev1", &result);
    assert_eq!(ack["status"], "failed");
    assert!(ack.as_object().unwrap().get("data").is_none());
}

#[test]
fn parse_commands_reads_the_array() {
    let body = serde_json::json!({
        "commands": [
            {"_id": "c1", "command": "get_services"},
            {"_id": "c2", "command": "plugin.enable", "args": {"pluginId": "p"}}
        ]
    });
    let cmds = parse_commands(&body);
    assert_eq!(cmds.len(), 2);
    assert_eq!(command_id(&cmds[0]), "c1");
    assert_eq!(command_name(&cmds[1]), "plugin.enable");
}

#[test]
fn parse_commands_empty_when_missing() {
    assert!(parse_commands(&serde_json::json!({})).is_empty());
}

/// The delivery window is measured from when the claim was sent, by the
/// window left at the queue's claim stamp, so the two clocks never meet.
#[tokio::test(start_paused = true)]
async fn the_run_deadline_counts_the_window_left_from_the_claim() {
    let sent = tokio::time::Instant::now();
    let row = serde_json::json!({"claimedAt": 5_000_000.0, "expiresAt": 5_004_000, "createdAt": 4_999_000});
    assert_eq!(
        run_deadline(&row, sent),
        Some(sent + Duration::from_secs(4))
    );
    let no_ttl = serde_json::json!({"claimedAt": 5_000_000, "createdAt": 4_999_000});
    assert_eq!(
        run_deadline(&no_ttl, sent),
        Some(sent + Duration::from_secs(599))
    );
    let past = serde_json::json!({"claimedAt": 5_000_000, "expiresAt": 4_000_000});
    assert_eq!(run_deadline(&past, sent), Some(sent));
    assert_eq!(
        run_deadline(&serde_json::json!({"expiresAt": 1}), sent),
        None
    );
}

/// A queue answering every ack with one status.
struct Answers(AckAttempt, std::cell::Cell<u32>);

impl CommandLink for Answers {
    async fn claim(&self) -> Option<serde_json::Value> {
        None
    }

    async fn ack(&self, _ack: &serde_json::Value) -> AckAttempt {
        self.1.set(self.1.get() + 1);
        self.0
    }
}

/// An ack is settled by a 2xx or at once by a refusal no retry changes (a
/// gone row's 404 among them); a 5xx, a 408 or a 429 is a refusal and a
/// transport failure goes unanswered, both kept for the next pass.
#[tokio::test(start_paused = true)]
async fn an_ack_is_kept_until_the_queue_answers_it_for_good() {
    let ack = serde_json::json!({"commandId": "c1"});
    for (answer, fate, tries) in [
        (AckAttempt::Delivered, AckFate::Settled, 1),
        (AckAttempt::Refused(404), AckFate::Settled, 1),
        (AckAttempt::Refused(400), AckFate::Settled, 1),
        (AckAttempt::Refused(429), AckFate::Refused, 3),
        (AckAttempt::Refused(408), AckFate::Refused, 3),
        (AckAttempt::Refused(503), AckFate::Refused, 3),
        (AckAttempt::Unanswered, AckFate::Unanswered, 3),
    ] {
        let link = Answers(answer, std::cell::Cell::new(0));
        assert_eq!(post_ack(&link, &ack).await, fate, "{answer:?}");
        assert_eq!(link.1.get(), tries, "{answer:?}");
    }
}
