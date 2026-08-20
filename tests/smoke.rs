//! End to end, over real JSON-RPC, against the real binary.

mod support;

use std::time::Duration;

use serde_json::json;
use support::{Sandbox, tool_text};

const PATIENCE: Duration = Duration::from_secs(30);

#[test]
fn a_cord_stops_the_agent_summons_a_human_and_returns_their_guidance() {
    let sandbox = Sandbox::new("smoke");
    let mut server = sandbox.serve();
    server.initialize();

    // Leave the call outstanding: an outstanding tool call *is* a stopped line.
    let pending = server.request(
        "tools/call",
        json!({
            "name": "pull_andon_cord",
            "arguments": { "report": "I need the staging database password." },
        }),
    );

    let id = sandbox.wait_for_cord();
    let listing = sandbox.andon(&["list"]);
    let listed = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listed.contains(&id),
        "andon list should show {id}: {listed}"
    );
    assert!(listed.contains("open"), "{listed}");
    assert!(listed.contains("staging database password"), "{listed}");

    // Answer from what is, as far as the server is concerned, another terminal.
    let answered = sandbox.andon(&["respond", &id[..2], "use the shared vault entry"]);
    assert!(answered.status.success(), "{answered:?}");
    let echoed = String::from_utf8_lossy(&answered.stdout);
    assert!(
        echoed.contains("staging database password"),
        "respond echoes what it answered, so a recycled id is visible: {echoed}"
    );

    let response = server.response(pending, PATIENCE);
    let text = tool_text(&response);
    assert!(text.contains("use the shared vault entry"), "{text}");
    assert!(response["result"]["isError"] != json!(true), "{response}");

    // The reader closes the loop: the hot path is empty and history is
    // timestamped, so two same-id cords from different days cannot collide.
    assert!(
        sandbox.cords().is_empty(),
        "cords/ must hold live cords only"
    );
    let archived = sandbox.archive();
    assert_eq!(archived.len(), 1, "{archived:?}");
    assert!(
        archived[0].ends_with(&format!("-{id}.json")),
        "{archived:?}"
    );
    assert!(archived[0].starts_with(char::is_numeric), "{archived:?}");
}

#[test]
fn the_cord_takes_anything_at_all() {
    let sandbox = Sandbox::new("smoke-anything");
    let mut server = sandbox.serve();
    server.initialize();

    // A rejected pull_andon_cord is the worst bug this tool could have: an
    // agent that gets a validation error back falls through to exactly the
    // behaviour the cord exists to prevent.
    let shapes = [
        json!({ "report": "a bare sentence" }),
        json!({ "report": { "trying": "deploy", "blocker": "no token" } }),
        json!({ "why": "splat", "of": ["top", "level"], "keys": 3 }),
        json!({ "report": ["a", "list"] }),
        json!({ "report": null }),
        json!({}),
    ];
    for arguments in shapes {
        let pending = server.request(
            "tools/call",
            json!({ "name": "pull_andon_cord", "arguments": arguments.clone() }),
        );
        let id = sandbox.wait_for_cord();
        assert_eq!(
            sandbox.cord_json(&id)["status"]["type"],
            "open",
            "{arguments} must open a cord"
        );
        assert!(
            sandbox
                .andon(&["respond", &id, "carry on"])
                .status
                .success()
        );

        let response = server.response(pending, PATIENCE);
        assert!(
            response.get("error").is_none(),
            "{arguments} was rejected: {response}"
        );
        assert!(tool_text(&response).contains("carry on"));
    }
}

#[test]
fn the_description_is_the_product_and_the_schema_rejects_nothing() {
    let sandbox = Sandbox::new("smoke-schema");
    let mut server = sandbox.serve();
    server.initialize();

    let tools = server.tools();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, vec!["pull_andon_cord", "await_cord"]);

    let pull = &tools[0];
    let schema = &pull["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], json!(true));
    assert!(schema.get("required").is_none(), "nothing may be required");
    assert!(
        schema["properties"]["report"].get("type").is_none(),
        "report must accept any shape"
    );

    let description = pull["description"].as_str().unwrap_or_default();
    assert!(description.contains("never a failure"), "{description}");
    assert!(
        description.contains("blocks until a human answers"),
        "{description}"
    );
}

#[test]
fn the_description_is_configurable_without_restating_the_framing() {
    let mut sandbox = Sandbox::new("smoke-description");
    sandbox.config(
        r#"{ "notify": [],
             "description_append": "Say which ticket you are on." }"#,
    );

    let printed = sandbox.andon(&["description"]);
    let printed = String::from_utf8_lossy(&printed.stdout);
    assert!(
        printed.contains("never a failure"),
        "the framing survives an append"
    );
    assert!(printed.trim_end().ends_with("Say which ticket you are on."));

    let mut server = sandbox.serve();
    server.initialize();
    let tools = server.tools();
    let served = tools[0]["description"].as_str().unwrap_or_default();
    assert_eq!(
        served.trim(),
        printed.trim(),
        "`andon description` shows what agents see"
    );
}

#[test]
fn clearing_a_cord_releases_the_agent_without_an_answer() {
    let sandbox = Sandbox::new("smoke-clear");
    let mut server = sandbox.serve();
    server.initialize();

    let pending = server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "stuck" } }),
    );
    let id = sandbox.wait_for_cord();
    assert!(
        sandbox
            .andon(&["clear", &id, "--reason", "handled out of band"])
            .status
            .success()
    );

    let text = tool_text(&server.response(pending, PATIENCE));
    assert!(text.contains("handled out of band"), "{text}");
    assert!(text.contains("end your turn"), "{text}");
    assert!(sandbox.cords().is_empty());
}

#[test]
fn tools_list_satisfies_every_protocol_version_a_client_might_negotiate() {
    // Regression: real clients negotiate the newest version they know, and from
    // 2026-07-28 SEP-2549 makes `ttlMs` and `cacheScope` required on paginated
    // results. Omitting them made Claude Code reject the entire tool list —
    // "Connected · tools fetch failed" — which no test pinned to an older
    // version could ever have caught.
    for protocol in support::KNOWN_PROTOCOLS {
        let sandbox = Sandbox::new(&format!("smoke-proto-{protocol}"));
        let mut server = sandbox.serve();
        let init = server.initialize_at(protocol, json!({}));
        assert!(
            init.get("error").is_none(),
            "initialize failed at {protocol}: {init}"
        );

        let result = server.tools_result();
        assert_eq!(
            result["tools"].as_array().map(Vec::len),
            Some(2),
            "at {protocol}: {result}"
        );
        assert!(
            result["ttlMs"].is_number(),
            "ttlMs must be a number at {protocol}: {result}"
        );
        assert!(
            matches!(result["cacheScope"].as_str(), Some("public" | "private")),
            "cacheScope must be public or private at {protocol}: {result}"
        );
    }
}

#[test]
fn a_cord_can_be_pulled_at_the_newest_protocol_version() {
    // The whole loop, not just the handshake, on the version a current client
    // actually picks.
    let sandbox = Sandbox::new("smoke-newest");
    let mut server = sandbox.serve();
    let newest = support::KNOWN_PROTOCOLS.last().unwrap();
    server.initialize_at(newest, json!({}));

    let pending = server.request(
        "tools/call",
        json!({ "name": "pull_andon_cord", "arguments": { "report": "stuck on the newest wire" } }),
    );
    let id = sandbox.wait_for_cord();
    assert!(
        sandbox
            .andon(&["respond", &id, "answered anyway"])
            .status
            .success()
    );

    let response = server.response(pending, PATIENCE);
    assert!(response.get("error").is_none(), "{response}");
    assert!(tool_text(&response).contains("answered anyway"));
}
