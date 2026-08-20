//! The line stop.
//!
//! Two layers, deliberately separated: [`decide`] is the harness-neutral
//! matching decision, and [`claude_code`] is a thin adapter that parses Claude
//! Code's `PreToolUse` payload and emits its decision JSON over the same
//! function. Supporting a new harness is then one small adapter rather than a
//! redesign.
//!
//! Nothing here is async and nothing here writes to the state directory. This
//! runs on every tool call.

use std::io::Read;
use std::process::ExitCode;

use serde_json::json;

use crate::board;
use crate::config::{Config, Scope};
use crate::cord::Cord;

/// What a harness knows about the tool call it is about to make. Everything is
/// optional because not every harness reports everything, and a missing field
/// must widen toward allowing.
#[derive(Debug, Default, Clone)]
pub struct Query {
    pub tool_name: Option<String>,
    pub session: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    Deny { cord_id: String, reason: String },
}

/// Denying requires everything to have gone right: the guard is on, a cord
/// exists, it is open, its agent is alive, it is in scope, and the call is not
/// one of ours. Every other path allows.
pub fn decide(query: &Query, config: &Config) -> Decision {
    if !config.guard {
        return Decision::Allow;
    }
    // Denying the andon tools while a cord is open would deadlock the agent out
    // of `await_cord`: it could neither work nor wait.
    if query.tool_name.as_deref().is_some_and(is_andon_tool) {
        return Decision::Allow;
    }
    let Some(cord) = board::stopping()
        .into_iter()
        .find(|cord| in_scope(cord, query, config.scope))
    else {
        return Decision::Allow;
    };
    Decision::Deny {
        reason: reason(&cord, query),
        cord_id: cord.id,
    }
}

/// Generous on purpose. The canonical name is `mcp__andon__*`, but the server
/// can be registered under any name, and a rename that locked the agent out of
/// its own cord would be the worst bug this tool could have.
fn is_andon_tool(name: &str) -> bool {
    name.starts_with("mcp__andon__")
        || name.ends_with("pull_andon_cord")
        || name.ends_with("await_cord")
}

fn in_scope(cord: &Cord, query: &Query, scope: Scope) -> bool {
    match scope {
        Scope::Global => true,
        // A missing identity cannot be matched, and an unmatchable cord allows.
        Scope::Session => query.session.as_deref().is_some_and(|s| s == cord.session),
        Scope::Project => query
            .cwd
            .as_deref()
            .is_some_and(|c| same_project(c, &cord.cwd)),
    }
}

/// Same directory, or one inside the other. A cord pulled at the repo root
/// should still stop work happening in a subdirectory of it.
fn same_project(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
    a == b || nested(a, b) || nested(b, a)
}

fn nested(inner: &str, outer: &str) -> bool {
    inner
        .strip_prefix(outer)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The only text the agent gets back, so it has to say what happened and what
/// to do. A denied call costs a full turn, so for anyone who is not the puller
/// the instruction is to stop — never to try something else, which would spin
/// against the guard and burn a turn on every bounce.
fn reason(cord: &Cord, query: &Query) -> String {
    let mine = query.session.as_deref().is_some_and(|s| s == cord.session);
    if mine {
        format!(
            "The line is stopped: andon cord {} is open and waiting on a human. \
             Do not work around this. Call await_cord(\"{}\") to keep waiting for their guidance.",
            cord.id, cord.id
        )
    } else {
        format!(
            "The line is stopped: andon cord {} is open in {} and waiting on a human. \
             Stop here and end your turn — report that you are blocked on cord {}. \
             Do not retry this call and do not attempt another approach; every \
             attempt will be denied until a human answers.",
            cord.id, cord.cwd, cord.id
        )
    }
}

/// Claude Code's `PreToolUse` adapter.
///
/// Exits 2 with the decision on stdout to deny, and 0 on every other path.
/// Claude Code blocks on exit 2 and reads the JSON for the message it shows the
/// model; every other non-zero exit is logged and the call proceeds. That is
/// what gives the failure mode for free — delete this binary with the hook
/// still installed and the shell returns 127, which is not 2, so every tool
/// call carries on.
///
/// A panic exits 101, which is likewise not 2. Failing open is the default here
/// rather than something to remember.
pub fn claude_code() -> ExitCode {
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        return ExitCode::SUCCESS;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload) else {
        return ExitCode::SUCCESS;
    };
    let string = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let query = Query {
        tool_name: string("tool_name"),
        session: string("session_id"),
        cwd: string("cwd"),
    };
    emit(decide(&query, &Config::load()))
}

fn emit(decision: Decision) -> ExitCode {
    let Decision::Deny { reason, .. } = decision else {
        return ExitCode::SUCCESS;
    };
    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
    );
    // Also on stderr, which is what Claude Code falls back to if it ever stops
    // recognising the JSON shape above.
    eprintln!("{reason}");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cord::CordStatus;
    use crate::testing::Sandbox;
    use serde_json::json;

    /// A pid nothing is using, which is how you name a process that is gone.
    const GONE: u32 = 0x7FFF_FFFE;

    fn plant(id: &str, session: &str, cwd: &str) -> Cord {
        let mut cord = Cord::new(
            id.into(),
            session.into(),
            cwd.into(),
            std::process::id(),
            json!("need the staging password"),
        );
        cord.id = id.to_string();
        board::save(&cord).expect("plant");
        cord
    }

    fn query(tool: &str, session: &str, cwd: &str) -> Query {
        Query {
            tool_name: Some(tool.into()),
            session: Some(session.into()),
            cwd: Some(cwd.into()),
        }
    }

    fn config(scope: Scope) -> Config {
        Config {
            scope,
            ..Config::default()
        }
    }

    #[test]
    fn an_open_cord_in_the_same_session_stops_the_line() {
        let _sandbox = Sandbox::new("guard-deny");
        plant("k7f2", "sess-a", "/repo");
        let decision = decide(&query("Bash", "sess-a", "/repo"), &config(Scope::Session));
        let Decision::Deny { cord_id, reason } = decision else {
            unreachable!()
        };
        assert_eq!(cord_id, "k7f2");
        assert!(reason.contains("await_cord(\"k7f2\")"), "{reason}");
    }

    #[test]
    fn another_session_is_told_to_stop_rather_than_to_retry() {
        let _sandbox = Sandbox::new("guard-other-session");
        plant("k7f2", "sess-a", "/repo");

        // Under the default scope it is not this session's problem at all.
        let elsewhere = query("Bash", "sess-b", "/other");
        assert_eq!(
            decide(&elsewhere, &config(Scope::Session)),
            Decision::Allow,
            "a cord in one repo must not freeze an unrelated session"
        );

        // Under global scope it is, and then the advice must end the turn: a
        // denial costs a full turn, so anything that invites a retry burns
        // context on every bounce.
        let Decision::Deny { reason, .. } = decide(&elsewhere, &config(Scope::Global)) else {
            panic!("global scope stops everything");
        };
        assert!(reason.contains("end your turn"), "{reason}");
        assert!(reason.contains("Do not retry"), "{reason}");
        assert!(
            !reason.contains("await_cord"),
            "not this session's cord: {reason}"
        );
    }

    #[test]
    fn project_scope_reaches_into_subdirectories() {
        let _sandbox = Sandbox::new("guard-project");
        plant("k7f2", "sess-a", "/repo");
        let config = config(Scope::Project);
        assert!(matches!(
            decide(&query("Bash", "sess-b", "/repo"), &config),
            Decision::Deny { .. }
        ));
        assert!(matches!(
            decide(&query("Bash", "sess-b", "/repo/src/deep"), &config),
            Decision::Deny { .. }
        ));
        assert_eq!(
            decide(&query("Bash", "sess-b", "/repo-other"), &config),
            Decision::Allow
        );
        assert_eq!(
            decide(&query("Bash", "sess-b", "/elsewhere"), &config),
            Decision::Allow
        );
    }

    #[test]
    fn our_own_tools_are_always_allowed() {
        let _sandbox = Sandbox::new("guard-self");
        plant("k7f2", "sess-a", "/repo");
        let config = config(Scope::Global);
        // Denying these would deadlock the agent out of its own cord: it could
        // neither work nor wait.
        for tool in [
            "mcp__andon__await_cord",
            "mcp__andon__pull_andon_cord",
            // Registered under some other server name, which must still work.
            "mcp__stopline__await_cord",
        ] {
            assert_eq!(
                decide(&query(tool, "sess-a", "/repo"), &config),
                Decision::Allow,
                "{tool}"
            );
        }
    }

    #[test]
    fn a_dead_agents_cord_never_blocks() {
        let _sandbox = Sandbox::new("guard-dead");
        let mut cord = plant("k7f2", "sess-a", "/repo");
        cord.pid = GONE;
        board::save(&cord).unwrap();
        assert_eq!(
            decide(&query("Bash", "sess-a", "/repo"), &config(Scope::Session)),
            Decision::Allow,
            "a crash mid-cord must not wedge the guard forever"
        );
    }

    #[test]
    fn a_resolved_cord_never_blocks() {
        let _sandbox = Sandbox::new("guard-resolved");
        let mut cord = plant("k7f2", "sess-a", "/repo");
        for status in [
            CordStatus::Answered {
                guidance: "do X".into(),
                answered_at: crate::cord::now(),
            },
            CordStatus::Abandoned {
                waited: std::time::Duration::from_secs(1),
                at: crate::cord::now(),
            },
            CordStatus::Cleared {
                reason: "nope".into(),
                cleared_at: crate::cord::now(),
            },
        ] {
            cord.status = status;
            board::save(&cord).unwrap();
            assert_eq!(
                decide(&query("Bash", "sess-a", "/repo"), &config(Scope::Session)),
                Decision::Allow,
                "{:?} must let the agent end its turn",
                cord.status
            );
        }
    }

    // The fail-open cases matter more than the deny path: a broken andon
    // install must never brick every agent on the machine.

    #[test]
    fn the_off_switch_allows_without_touching_the_hook() {
        let _sandbox = Sandbox::new("guard-off");
        plant("k7f2", "sess-a", "/repo");
        let off = Config {
            guard: false,
            ..Config::default()
        };
        assert_eq!(
            decide(&query("Bash", "sess-a", "/repo"), &off),
            Decision::Allow
        );
    }

    #[test]
    fn an_unreadable_state_dir_allows() {
        let sandbox = Sandbox::new("guard-nostate");
        sandbox.set("ANDON_STATE_DIR", sandbox.dir.join("does/not/exist"));
        assert_eq!(
            decide(&query("Bash", "sess-a", "/repo"), &config(Scope::Global)),
            Decision::Allow
        );
    }

    #[test]
    fn a_cord_full_of_garbage_allows() {
        let _sandbox = Sandbox::new("guard-garbage");
        std::fs::create_dir_all(board::cords_dir()).unwrap();
        std::fs::write(board::cords_dir().join("k7f2.json"), b"\xff\xfe not json").unwrap();
        assert_eq!(
            decide(&query("Bash", "sess-a", "/repo"), &config(Scope::Global)),
            Decision::Allow
        );
    }

    #[test]
    fn an_identity_we_cannot_match_allows() {
        let _sandbox = Sandbox::new("guard-noidentity");
        plant("k7f2", "sess-a", "/repo");
        let blind = Query {
            tool_name: Some("Bash".into()),
            session: None,
            cwd: None,
        };
        assert_eq!(decide(&blind, &config(Scope::Session)), Decision::Allow);
        assert_eq!(decide(&blind, &config(Scope::Project)), Decision::Allow);
    }
}
