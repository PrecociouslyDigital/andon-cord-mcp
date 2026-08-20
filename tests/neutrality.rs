//! Harness neutrality.
//!
//! The pause is the MCP protocol, not anything we built, so the core has to
//! work with no harness at all. These tests run with every `CLAUDE_*` variable
//! stripped from the environment.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{Sandbox, tool_text};

const PATIENCE: Duration = Duration::from_secs(30);

#[test]
fn with_no_harness_at_all_a_cord_still_opens_notifies_and_resolves() {
    let mut sandbox = Sandbox::new("neutral");
    let touched = sandbox.dir.join("summoned");
    // A notifier we can observe without ringing anyone's terminal.
    sandbox.config(&format!(
        r#"{{ "notify": [ {{ "type": "command",
                             "argv": ["/bin/sh", "-c", "printf '%s' \"$1\" > {}", "andon", "{{{{id}}}} {{{{report}}}}"] }} ] }}"#,
        touched.display()
    ));

    let mut server = sandbox.serve();
    server.initialize();
    let pending = server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "no harness here" } }),
    );
    let id = sandbox.wait_for_cord();

    // The identity chain lands on the pid fallback, which is a real identity
    // rather than a degradation: a stdio server is spawned once per session, so
    // its process genuinely is the session.
    assert_eq!(
        sandbox.cord_json(&id)["session"],
        json!(format!("pid:{}", server.pid()))
    );

    let summoned = wait_for_file(&touched);
    assert!(summoned.contains(&id), "the notifier fired: {summoned:?}");
    assert!(summoned.contains("no harness here"), "{summoned:?}");

    assert!(
        sandbox
            .andon(&["respond", &id, "carry on"])
            .status
            .success()
    );
    assert!(tool_text(&server.response(pending, PATIENCE)).contains("carry on"));
    assert!(sandbox.cords().is_empty());
    assert_eq!(sandbox.archive().len(), 1);
}

#[test]
fn a_client_that_advertises_nothing_still_gets_the_tools() {
    // No elicitation, no progress token, no sampling: the floor of MCP.
    let sandbox = Sandbox::new("neutral-bare");
    let mut server = sandbox.serve();
    server.initialize();
    let names: Vec<String> = server
        .tools()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(names, vec!["pull_andon_cord", "await_cord"]);
}

#[test]
fn progress_heartbeats_are_sent_only_when_the_client_asked_for_them() {
    let mut sandbox = Sandbox::new("neutral-progress");
    sandbox.config(r#"{ "notify": [], "max_wait": "25s" }"#);
    let mut server = sandbox.serve();
    server.initialize();

    // A progressToken in _meta is the client saying it wants heartbeats.
    let pending = server.request(
        "tools/call",
        json!({
            "name": "pull_andon_cord",
            "arguments": { "report": "waiting a while" },
            "_meta": { "progressToken": "beat" },
        }),
    );
    let id = sandbox.wait_for_cord();

    // Heartbeats cost zero model tokens: they are transport-level
    // server→client notifications that never enter the conversation.
    let beats = wait_for_progress(&mut server);
    assert_eq!(beats["params"]["progressToken"], "beat");
    assert!(
        beats["params"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(&id),
        "{beats}"
    );

    assert!(sandbox.andon(&["respond", &id, "done"]).status.success());
    assert!(tool_text(&server.response(pending, PATIENCE)).contains("done"));
}

fn wait_for_file(path: &std::path::Path) -> String {
    for _ in 0..200 {
        if let Ok(body) = std::fs::read_to_string(path)
            && !body.is_empty()
        {
            return body;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("nothing wrote {}", path.display());
}

fn wait_for_progress(server: &mut support::Server) -> serde_json::Value {
    for _ in 0..120 {
        if let Some(beat) = server
            .notifications()
            .into_iter()
            .find(|n| n["method"] == "notifications/progress")
        {
            return beat;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("no progress notification arrived");
}
