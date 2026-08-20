//! Summoning a human.
//!
//! The configurability here comes from one idea rather than five: a single
//! [`substitute`] that walks any JSON replacing `{{…}}`, applied uniformly to a
//! webhook body, a tmux message, or a command's argv. Discord, Slack and ntfy
//! fall out of that without any of them being special-cased.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::cord::{Cord, render, truncate};

/// Long enough to be useful in a notification, short enough for a phone.
const REPORT_LIMIT: usize = 600;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notifier {
    Bell,
    Tmux {
        message: String,
    },
    Webhook {
        url: String,
        body: Value,
    },
    Desktop {
        title: String,
        message: String,
    },
    /// The universal escape hatch.
    Command {
        argv: Vec<String>,
    },
}

impl Notifier {
    /// A webhook body that suits Discord (`content`), Slack (`text`) and ntfy
    /// (either) at once — each ignores the key it doesn't know. Used for the
    /// zero-config `ANDON_WEBHOOK_URL` path.
    pub fn webhook(url: String) -> Notifier {
        let line = "🔴 Andon cord {{id}} pulled in `{{cwd}}`\n{{report}}\n\n`andon respond {{id}}`";
        Notifier::Webhook {
            url,
            body: json!({ "content": line, "text": line }),
        }
    }

    /// The default desktop summons. A terminal bell is easy to miss under a
    /// full-screen TUI — and on a stdio MCP server it rings the client's
    /// terminal, not the human's attention — so the shipped default also raises
    /// something the operating system puts in front of them.
    pub fn desktop() -> Notifier {
        Notifier::Desktop {
            title: "🔴 Andon cord {{id}}".to_string(),
            message: "{{report}}".to_string(),
        }
    }

    fn fire(&self, cord: &Cord) -> Result<(), String> {
        match self {
            Notifier::Bell => {
                bell();
                Ok(())
            }
            Notifier::Tmux { message } => {
                let message = fill(message, cord);
                run(&["tmux", "display-message", &message])?;
                // tmux raises the window's bell flag when a pane rings, which is
                // what makes the window name stand out in the status line.
                bell();
                Ok(())
            }
            Notifier::Webhook { url, body } => post(&fill(url, cord), &substitute(body, cord)),
            Notifier::Desktop { title, message } => {
                desktop(&fill(title, cord), &fill(message, cord))
            }
            Notifier::Command { argv } => {
                let argv: Vec<String> = argv.iter().map(|a| fill(a, cord)).collect();
                let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
                run(&argv)
            }
        }
    }
}

/// Fire every notifier at once. A failing notifier is logged and otherwise
/// ignored: failing to summon must never fail the cord.
///
/// Nothing is joined. A webhook pointed at a dead host must not hold up the
/// wait — the cord is already open by the time we get here, and the human it is
/// summoning is the only thing that resolves it.
pub fn notify_all(notifiers: &[Notifier], cord: &Cord) {
    for notifier in notifiers {
        let notifier = notifier.clone();
        let cord = cord.clone();
        std::thread::spawn(move || {
            if let Err(e) = notifier.fire(&cord) {
                eprintln!("andon: notifier failed: {e}");
            }
        });
    }
}

/// Walks any JSON value, filling `{{…}}` in every string it finds. Total by
/// construction: an unknown placeholder resolves to an empty string rather than
/// erroring, because a config written against one agent's reporting habits will
/// meet another agent that sends a bare sentence.
pub fn substitute(template: &Value, cord: &Cord) -> Value {
    match template {
        Value::String(s) => Value::String(fill(s, cord)),
        Value::Array(items) => Value::Array(items.iter().map(|i| substitute(i, cord)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, cord)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Replaces every `{{key}}` in one string. An unterminated `{{` is left alone.
fn fill(template: &str, cord: &Cord) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let (before, after) = rest.split_at(start);
        out.push_str(before);
        match after[2..].find("}}") {
            Some(end) => {
                out.push_str(&lookup(cord, after[2..2 + end].trim()));
                rest = &after[2 + end + 2..];
            }
            None => {
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The cord envelope always resolves; dotted paths reach into the free-form
/// report and resolve to nothing when the agent didn't send that shape.
fn lookup(cord: &Cord, key: &str) -> String {
    match key {
        "id" => cord.id.clone(),
        "cwd" => cord.cwd.clone(),
        "session" => cord.session.clone(),
        "pid" => cord.pid.to_string(),
        "pulled_at" => crate::cord::format_utc(cord.pulled_at),
        "report" => truncate(&render(&cord.report), REPORT_LIMIT),
        _ => key
            .strip_prefix("report.")
            .and_then(|path| dig(&cord.report, path))
            .unwrap_or_default(),
    }
}

/// Follows a dotted path into a value, indexing arrays by number.
fn dig(value: &Value, path: &str) -> Option<String> {
    let mut cursor = value;
    for segment in path.split('.') {
        cursor = match cursor {
            Value::Object(map) => map.get(segment)?,
            Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(render(cursor))
}

/// `\a` to the terminal, falling back to stderr when there isn't one.
fn bell() {
    let wrote = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .and_then(|mut tty| tty.write_all(b"\x07"))
        .is_ok();
    if !wrote {
        let _ = std::io::stderr().write_all(b"\x07");
    }
}

fn run(argv: &[&str]) -> Result<(), String> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("{program}: {e}"))?;
    match status.success() {
        true => Ok(()),
        false => Err(format!("{program} exited with {status}")),
    }
}

fn desktop(title: &str, message: &str) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let script = format!(
            "display notification {} with title {}",
            applescript_string(message),
            applescript_string(title)
        );
        run(&["osascript", "-e", &script])
    } else {
        run(&["notify-send", title, message])
    }
}

/// AppleScript string literals escape only backslash and double quote.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A dead webhook must not pin a thread for the rest of the session, so the
/// whole request is capped.
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(15);

fn post(url: &str, body: &Value) -> Result<(), String> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(WEBHOOK_TIMEOUT))
            .build(),
    );
    let status = agent
        .post(url)
        .header("content-type", "application/json")
        .send(body.to_string())
        .map_err(|e| format!("POST {url}: {e}"))?
        .status();
    match status.is_success() {
        true => Ok(()),
        false => Err(format!("POST {url}: {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cord::Cord;
    use crate::testing::arb_value;
    use proptest::prelude::*;

    fn cord(report: Value) -> Cord {
        let mut cord = Cord::new("k7f2".into(), "sess-a".into(), "/repo".into(), 4242, report);
        cord.pulled_at = crate::cord::from_millis(1_700_000_000_000);
        cord
    }

    fn filled(template: &str, report: Value) -> String {
        fill(template, &cord(report))
    }

    #[test]
    fn the_envelope_always_resolves() {
        let out = filled(
            "{{id}} {{cwd}} {{session}} {{pid}} {{pulled_at}}",
            json!("hi"),
        );
        assert_eq!(out, "k7f2 /repo sess-a 4242 2023-11-14T22:13:20Z");
    }

    #[test]
    fn a_bare_string_report_reads_as_prose() {
        assert_eq!(
            filled("{{report}}", json!("no staging password")),
            "no staging password"
        );
    }

    #[test]
    fn an_object_report_reads_as_key_value_lines() {
        let report = json!({ "trying": "deploy", "blocker": "no token" });
        assert_eq!(
            filled("{{report}}", report),
            "trying: deploy\nblocker: no token"
        );
    }

    #[test]
    fn dotted_paths_reach_into_the_report() {
        let report = json!({ "blocker": "no token", "tried": ["env", "keychain"] });
        assert_eq!(filled("{{report.blocker}}", report.clone()), "no token");
        assert_eq!(filled("{{report.tried.1}}", report), "keychain");
    }

    #[test]
    fn templates_degrade_quietly_instead_of_erroring() {
        // A config written against one agent's reporting habits will meet
        // another agent that sends a bare sentence.
        let bare = json!("just stuck");
        assert_eq!(filled("[{{report.blocker}}]", bare.clone()), "[]");
        assert_eq!(filled("[{{nonsense}}]", bare.clone()), "[]");
        assert_eq!(filled("[{{unclosed", bare), "[{{unclosed");
    }

    #[test]
    fn substitution_reaches_every_string_in_a_body() {
        let body = json!({
            "content": "cord {{id}}",
            "embeds": [{ "title": "{{report.blocker}}", "colour": 16711680 }],
        });
        let out = substitute(&body, &cord(json!({ "blocker": "no token" })));
        assert_eq!(
            out,
            json!({
                "content": "cord k7f2",
                "embeds": [{ "title": "no token", "colour": 16711680 }],
            })
        );
    }

    #[test]
    fn a_long_report_is_cut_to_fit_a_notification() {
        let long = "x".repeat(REPORT_LIMIT * 2);
        let out = filled("{{report}}", json!(long));
        assert_eq!(out.chars().count(), REPORT_LIMIT);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn the_zero_config_webhook_speaks_discord_and_slack_at_once() {
        let Notifier::Webhook { body, .. } = Notifier::webhook("https://example.invalid".into())
        else {
            panic!("expected a webhook")
        };
        let out = substitute(&body, &cord(json!("stuck")));
        assert_eq!(out["content"], out["text"]);
        assert!(
            out["content"]
                .as_str()
                .unwrap()
                .contains("andon respond k7f2")
        );
    }

    proptest! {
        /// Substitution is total: it never panics, and it never changes the
        /// shape of the body it is filling in.
        #[test]
        fn substitute_preserves_shape(body in arb_value(), report in arb_value()) {
            let out = substitute(&body, &cord(report));
            prop_assert!(same_shape(&body, &out));
        }

        /// Whatever the report, the envelope placeholders still resolve.
        #[test]
        fn the_envelope_survives_any_report(report in arb_value()) {
            prop_assert_eq!(filled("{{id}}", report), "k7f2");
        }
    }

    fn same_shape(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::String(_), Value::String(_)) => true,
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_shape(a, b))
            }
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|((ak, av), (bk, bv))| ak == bk && same_shape(av, bv))
            }
            // Non-strings are carried through untouched.
            (a, b) => a == b,
        }
    }
}
