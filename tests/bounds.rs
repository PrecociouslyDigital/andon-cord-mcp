//! Token-spend bounds.
//!
//! Waiting is free — the turn is suspended and nothing is re-sent — but
//! *re-entering* costs a full turn, and so does every guard denial. This is the
//! regression test for the failure mode that actually costs money: an agent
//! that spins instead of stopping.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{Sandbox, tool_text};

const PATIENCE: Duration = Duration::from_secs(60);

#[test]
fn an_unanswered_cord_re_enters_a_bounded_number_of_times_then_stops() {
    let mut sandbox = Sandbox::new("bounds");
    sandbox.config(r#"{ "notify": [], "max_wait": "1s", "max_reentries": 2 }"#);
    let mut server = sandbox.serve();
    server.initialize();

    let pull = server.call(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "nobody is home" } }),
        PATIENCE,
    );
    let text = tool_text(&pull);
    assert!(
        text.contains("await_cord"),
        "the first block invites one re-entry: {text}"
    );
    let id = sandbox.wait_for_cord();

    // Exactly max_reentries re-entries, each buying more wall clock than the
    // last. The final one waits and *then* ends terminally, rather than
    // spending another whole turn just to say "give up".
    let mut waits = Vec::new();

    let started = std::time::Instant::now();
    let text = tool_text(&server.call(
        "tools/call",
        json!({ "name": "await_cord", "arguments": { "cord_id": id } }),
        PATIENCE,
    ));
    waits.push(started.elapsed());
    assert!(
        text.contains("await_cord"),
        "the first re-entry still waits: {text}"
    );
    assert_eq!(sandbox.cord_json(&id)["reentries"], json!(1));

    let started = std::time::Instant::now();
    let text = tool_text(&server.call(
        "tools/call",
        json!({ "name": "await_cord", "arguments": { "cord_id": id } }),
        PATIENCE,
    ));
    waits.push(started.elapsed());
    assert_eq!(
        sandbox.cord_json(&id)["reentries"],
        json!(2),
        "and no more than that"
    );
    assert!(
        waits[1] > waits[0],
        "backoff should buy more wall clock each turn: {waits:?}"
    );
    assert!(text.contains("end your turn"), "{text}");
    assert!(text.contains("Do not attempt a workaround"), "{text}");
    assert!(
        !text.contains("await_cord"),
        "nothing left to invite: {text}"
    );

    // An agent that gave up leaves its cord abandoned rather than open, because
    // the human needs to know nobody is listening any more.
    let cord = sandbox.cord_json(&id);
    assert_eq!(cord["status"]["type"], "abandoned");

    // And it stays terminal: calling again does not restart the wait.
    let started = std::time::Instant::now();
    let again = tool_text(&server.call(
        "tools/call",
        json!({ "name": "await_cord", "arguments": { "cord_id": id } }),
        PATIENCE,
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "it must not loop"
    );
    assert!(again.contains("end your turn"), "{again}");
}

#[test]
fn an_abandoned_cord_is_marked_and_answering_it_warns_first() {
    let mut sandbox = Sandbox::new("bounds-abandoned");
    sandbox.config(r#"{ "notify": [], "max_wait": "1s", "max_reentries": 0 }"#);
    let mut server = sandbox.serve();
    server.initialize();

    server.call(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "nobody is home" } }),
        PATIENCE,
    );
    let id = sandbox.wait_for_cord();
    assert_eq!(sandbox.cord_json(&id)["status"]["type"], "abandoned");

    let listed = sandbox.andon(&["list"]);
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.contains("abandoned"), "{listed}");
    assert!(listed.contains("a reply cannot reach it"), "{listed}");

    // Typing a careful answer into a cord that nothing is waiting on is a
    // small, specific, entirely avoidable misery. Warn, then write it anyway.
    let answered = sandbox.andon(&["respond", &id, "sorry, was away"]);
    assert!(answered.status.success());
    let warning = String::from_utf8_lossy(&answered.stderr);
    assert!(warning.contains("will not reach it"), "{warning}");
    assert_eq!(sandbox.cord_json(&id)["status"]["type"], "answered");
}

#[test]
fn a_denial_tells_an_unrelated_agent_to_stop_rather_than_to_try_again() {
    // A denied tool call is also a full turn, so under a wider scope an
    // unrelated session could otherwise spin against the guard.
    let mut sandbox = Sandbox::new("bounds-denial");
    sandbox.config(r#"{ "notify": [], "scope": "global" }"#);
    sandbox.set("ANDON_SESSION_ID", "sess-a");
    let mut server = sandbox.serve();
    server.initialize();
    server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "stuck" } }),
    );
    sandbox.wait_for_cord();

    let payload = json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess-b",
        "cwd": "/elsewhere",
        "tool_name": "Bash",
    })
    .to_string();
    let out = sandbox.andon_stdin(&["guard"], &payload);
    assert_eq!(out.status.code(), Some(2));

    let decision: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let reason = decision["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap_or_default();
    assert!(reason.contains("end your turn"), "{reason}");
    assert!(reason.contains("Do not retry"), "{reason}");
}
