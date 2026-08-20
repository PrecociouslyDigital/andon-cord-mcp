//! Elicitation: answering in place, on clients that support it.
//!
//! It is opt-in, it races the state file, and either one answering is a
//! perfectly good outcome — so the interesting behaviour is what happens to the
//! loser.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{Sandbox, tool_text};

const PATIENCE: Duration = Duration::from_secs(30);

fn elicit_sandbox(name: &str) -> Sandbox {
    let mut sandbox = Sandbox::new(name);
    sandbox.config(r#"{ "notify": [], "elicit": true, "max_wait": "20s" }"#);
    sandbox
}

#[test]
fn a_dialog_answer_comes_back_as_the_tool_result() {
    let sandbox = elicit_sandbox("elicit-accept");
    let mut server = sandbox.serve();
    server.initialize_with(json!({ "elicitation": {} }));

    let pending = server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "which staging cluster?" } }),
    );

    let ask = server.server_request("elicitation/create", PATIENCE);
    let message = ask["params"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("which staging cluster?"), "{message}");
    assert!(
        ask["params"]["requestedSchema"]["properties"]["guidance"].is_object(),
        "the dialog asks for guidance: {ask}"
    );

    server.reply(
        &ask,
        json!({ "action": "accept", "content": { "guidance": "the eu-west one" } }),
    );

    let text = tool_text(&server.response(pending, PATIENCE));
    assert!(text.contains("the eu-west one"), "{text}");

    // However it was answered, the cord is resolved and out of the hot path.
    assert!(sandbox.cords().is_empty());
    assert_eq!(sandbox.archive().len(), 1);
    let archived = sandbox.archived_json(&sandbox.archive()[0]);
    assert_eq!(archived["status"]["type"], "answered");
    assert_eq!(archived["status"]["guidance"], "the eu-west one");
}

#[test]
fn the_state_file_can_win_the_race_while_a_dialog_is_open() {
    let sandbox = elicit_sandbox("elicit-race");
    let mut server = sandbox.serve();
    server.initialize_with(json!({ "elicitation": {} }));

    let pending = server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "stuck" } }),
    );
    // Let the dialog open, then answer from a terminal instead of clicking it.
    let _ask = server.server_request("elicitation/create", PATIENCE);
    let id = sandbox.wait_for_cord();
    assert!(
        sandbox
            .andon(&["respond", &id, "answered elsewhere"])
            .status
            .success()
    );

    let text = tool_text(&server.response(pending, PATIENCE));
    assert!(text.contains("answered elsewhere"), "{text}");
    assert!(sandbox.cords().is_empty());
}

#[test]
fn declining_the_dialog_keeps_waiting_rather_than_giving_up() {
    // A decline is not a failure: the human may still answer from a terminal,
    // so the cord stays open and the CLI still reaches it.
    let sandbox = elicit_sandbox("elicit-decline");
    let mut server = sandbox.serve();
    server.initialize_with(json!({ "elicitation": {} }));

    let pending = server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "stuck" } }),
    );
    let ask = server.server_request("elicitation/create", PATIENCE);
    server.reply(&ask, json!({ "action": "decline" }));

    let id = sandbox.wait_for_cord();
    assert!(
        sandbox
            .andon(&["respond", &id, "here you go"])
            .status
            .success()
    );
    let text = tool_text(&server.response(pending, PATIENCE));
    assert!(text.contains("here you go"), "{text}");
}

#[test]
fn no_dialog_is_opened_unless_it_is_switched_on() {
    let sandbox = Sandbox::new("elicit-off");
    let mut server = sandbox.serve();
    // The client supports it, but the config does not ask for it.
    server.initialize_with(json!({ "elicitation": {} }));

    let pending = server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "stuck" } }),
    );
    let id = sandbox.wait_for_cord();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !server
            .notifications()
            .iter()
            .any(|m| m["method"] == "elicitation/create"),
        "elicitation is opt-in"
    );

    assert!(sandbox.andon(&["respond", &id, "fine"]).status.success());
    assert!(tool_text(&server.response(pending, PATIENCE)).contains("fine"));
}
