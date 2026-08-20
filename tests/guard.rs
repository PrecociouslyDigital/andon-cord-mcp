//! The guard, fed the payloads a harness would send it.
//!
//! The fail-open cases matter more than the deny path: a broken andon install
//! must never brick every agent on the machine.

mod support;

use std::process::Output;

use serde_json::json;
use support::Sandbox;

/// A pid nothing is using.
const GONE: u32 = 0x7FFF_FFFE;

fn payload(tool: &str, session: &str, cwd: &str) -> String {
    json!({
        "hook_event_name": "PreToolUse",
        "session_id": session,
        "cwd": cwd,
        "tool_name": tool,
        "tool_input": { "command": "echo hi" },
    })
    .to_string()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

/// A sandbox with one open cord, pulled by a live server in session `sess-a`.
fn stopped(name: &str) -> (Sandbox, support::Server, String) {
    let mut sandbox = Sandbox::new(name);
    sandbox.set("ANDON_SESSION_ID", "sess-a");
    let mut server = sandbox.serve();
    server.initialize();
    server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "no credential" } }),
    );
    let id = sandbox.wait_for_cord();
    (sandbox, server, id)
}

#[test]
fn an_open_cord_denies_with_the_decision_claude_code_reads() {
    let (sandbox, _server, id) = stopped("guard-deny");
    let out = sandbox.andon_stdin(&["guard"], &payload("Bash", "sess-a", "/anywhere"));

    assert_eq!(code(&out), 2, "Claude Code blocks on exit 2");
    let decision: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("decision JSON on stdout");
    let hook = &decision["hookSpecificOutput"];
    assert_eq!(hook["hookEventName"], "PreToolUse");
    assert_eq!(hook["permissionDecision"], "deny");
    let reason = hook["permissionDecisionReason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains(&id),
        "the agent is told which cord: {reason}"
    );
    assert!(
        reason.contains("await_cord"),
        "and what to do about it: {reason}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stderr).is_empty(),
        "stderr carries the reason too, in case the JSON shape ever drifts"
    );
}

#[test]
fn another_session_is_left_running() {
    let (sandbox, _server, _id) = stopped("guard-scope");
    let out = sandbox.andon_stdin(&["guard"], &payload("Bash", "sess-b", "/elsewhere"));
    assert_eq!(
        code(&out),
        0,
        "session scope is the default for exactly this reason"
    );
}

#[test]
fn the_andon_tools_are_never_denied() {
    let (sandbox, _server, _id) = stopped("guard-self");
    for tool in ["mcp__andon__await_cord", "mcp__andon__pull_andon_cord"] {
        let out = sandbox.andon_stdin(&["guard"], &payload(tool, "sess-a", "/anywhere"));
        assert_eq!(
            code(&out),
            0,
            "{tool} must stay reachable or the agent deadlocks"
        );
    }
}

#[test]
fn a_cord_whose_agent_died_stops_blocking() {
    let (sandbox, server, id) = stopped("guard-dead");
    let mut cord = sandbox.cord_json(&id);
    cord["pid"] = json!(GONE);
    sandbox.set_cord(&id, &cord);
    drop(server);

    let out = sandbox.andon_stdin(&["guard"], &payload("Bash", "sess-a", "/anywhere"));
    assert_eq!(
        code(&out),
        0,
        "a crash mid-cord must not wedge the guard forever"
    );
}

#[test]
fn the_off_switch_works_without_touching_the_settings_file() {
    let (mut sandbox, _server, _id) = stopped("guard-off");
    sandbox.set("ANDON_GUARD", "0");
    let out = sandbox.andon_stdin(&["guard"], &payload("Bash", "sess-a", "/anywhere"));
    assert_eq!(
        code(&out),
        0,
        "ANDON_GUARD=0 is what you reach for mid-session"
    );
}

#[test]
fn everything_broken_still_allows() {
    let mut sandbox = Sandbox::new("guard-broken");
    sandbox.set("ANDON_SESSION_ID", "sess-a");
    let good = payload("Bash", "sess-a", "/anywhere");

    // No state directory at all.
    assert_eq!(code(&sandbox.andon_stdin(&["guard"], &good)), 0);

    // A state directory full of nonsense.
    std::fs::create_dir_all(sandbox.dir.join("state/cords")).unwrap();
    std::fs::write(sandbox.dir.join("state/cords/k7f2.json"), b"\xff not json").unwrap();
    assert_eq!(code(&sandbox.andon_stdin(&["guard"], &good)), 0);

    // A config that does not parse.
    sandbox.config("{ nope");
    assert_eq!(code(&sandbox.andon_stdin(&["guard"], &good)), 0);

    // A payload that does not parse, and one that is empty.
    assert_eq!(code(&sandbox.andon_stdin(&["guard"], "not json at all")), 0);
    assert_eq!(code(&sandbox.andon_stdin(&["guard"], "")), 0);
}

#[test]
fn check_and_guard_cannot_disagree() {
    // The two must not be able to give different verdicts: `check` is the
    // harness-neutral primitive and `guard` is the same decision wearing
    // Claude Code's clothes.
    let (sandbox, _server, _id) = stopped("guard-agree");
    for (tool, session, cwd) in [
        ("Bash", "sess-a", "/anywhere"),
        ("Bash", "sess-b", "/elsewhere"),
        ("mcp__andon__await_cord", "sess-a", "/anywhere"),
        ("Write", "sess-a", "/repo"),
    ] {
        let guard = sandbox.andon_stdin(&["guard"], &payload(tool, session, cwd));
        let check = sandbox.andon(&["check", "--session", session, "--cwd", cwd, "--tool", tool]);
        assert_eq!(
            code(&guard),
            code(&check),
            "guard and check disagreed about {tool} in {session}"
        );
    }
}
