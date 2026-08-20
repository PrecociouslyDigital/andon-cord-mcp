//! Editing someone else's settings file.
//!
//! Everything here works on `serde_json::Value` with `preserve_order`, mutating
//! only `hooks.PreToolUse`. Every unrelated key and its ordering survives
//! untouched, and the file is never rewritten from a typed struct — which would
//! silently discard whatever we don't happen to model.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Added,
    /// Found an existing entry and pointed it at a new path — which doubles as
    /// the fix for a moved or reinstalled binary.
    Updated,
    Unchanged,
    Removed,
    NotPresent,
}

impl Outcome {
    pub fn changed(self) -> bool {
        matches!(self, Outcome::Added | Outcome::Updated | Outcome::Removed)
    }
}

pub struct Edit {
    pub outcome: Outcome,
    pub rendered: String,
}

/// The entry we add. The matcher stays `"*"` and all the logic lives in the
/// guard: a trivial settings entry is easier to eyeball, easier to remove, and
/// keeps the interesting behaviour somewhere testable.
fn guard_hook(command: &str) -> Value {
    json!({ "type": "command", "command": command })
}

/// Recognises our own entry regardless of where the binary lives, so a second
/// install updates rather than duplicates.
fn is_ours(hook: &Value) -> bool {
    let Some(command) = hook.get("command").and_then(Value::as_str) else {
        return false;
    };
    let command = command.trim();
    let Some(program) = command.strip_suffix("guard").map(str::trim_end) else {
        return false;
    };
    // `guard` must be a separate argument, not the tail of a longer path.
    program.len() < command.len()
        && Path::new(program.trim_matches(['"', '\'']))
            .file_stem()
            .is_some_and(|stem| stem == "andon")
}

/// Idempotent: a second install finds the existing entry by its `andon guard`
/// command and updates its path rather than appending a duplicate.
pub fn install(settings: &Value, command: &str) -> Edit {
    let mut settings = settings.clone();
    let outcome = match find_ours(&mut settings) {
        Some(existing) => {
            if existing.get("command").and_then(Value::as_str) == Some(command) {
                Outcome::Unchanged
            } else {
                existing["command"] = Value::String(command.to_string());
                Outcome::Updated
            }
        }
        None => {
            groups(&mut settings).push(json!({
                "matcher": "*",
                "hooks": [guard_hook(command)],
            }));
            Outcome::Added
        }
    };
    Edit {
        rendered: render(&settings),
        outcome,
    }
}

/// Removes exactly what install added, then prunes the containing group and the
/// `PreToolUse` and `hooks` keys if and only if they are left empty — so the
/// file returns to its prior shape instead of accumulating
/// `{"hooks":{"PreToolUse":[]}}` litter. Other people's hooks are never touched.
pub fn uninstall(settings: &Value) -> Edit {
    let mut settings = settings.clone();
    let mut removed = false;

    if let Some(Value::Array(groups)) = pre_tool_use(&mut settings) {
        for group in groups.iter_mut() {
            if let Some(Value::Array(hooks)) = group.get_mut("hooks") {
                let before = hooks.len();
                hooks.retain(|hook| !is_ours(hook));
                removed |= hooks.len() != before;
            }
        }
        groups.retain(|group| !is_empty_group(group));
    }
    if removed {
        prune(&mut settings);
    }
    Edit {
        rendered: render(&settings),
        outcome: if removed {
            Outcome::Removed
        } else {
            Outcome::NotPresent
        },
    }
}

/// A group we emptied. A group that never had a `hooks` array is someone else's
/// business and is left alone.
fn is_empty_group(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
}

fn find_ours(settings: &mut Value) -> Option<&mut Value> {
    let Some(Value::Array(groups)) = pre_tool_use(settings) else {
        return None;
    };
    groups
        .iter_mut()
        .filter_map(|group| group.get_mut("hooks")?.as_array_mut())
        .flatten()
        .find(|hook| is_ours(hook))
}

fn pre_tool_use(settings: &mut Value) -> Option<&mut Value> {
    settings.get_mut("hooks")?.get_mut("PreToolUse")
}

/// The `hooks.PreToolUse` array, creating it if it isn't there yet.
fn groups(settings: &mut Value) -> &mut Vec<Value> {
    if !settings.is_object() {
        *settings = Value::Object(Map::new());
    }
    let hooks = settings
        .as_object_mut()
        .expect("just ensured object")
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let pre = hooks
        .as_object_mut()
        .expect("just ensured object")
        .entry("PreToolUse")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !pre.is_array() {
        *pre = Value::Array(Vec::new());
    }
    pre.as_array_mut().expect("just ensured array")
}

fn prune(settings: &mut Value) {
    let Some(root) = settings.as_object_mut() else {
        return;
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    if hooks
        .get("PreToolUse")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        hooks.remove("PreToolUse");
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
}

/// Two-space pretty with a trailing newline, which is what Claude Code writes.
fn render(settings: &Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(settings).unwrap_or_default()
    )
}

/// Which settings file to edit. The default is per-machine because this is a
/// per-machine tool; `--project` and `--local` exist for narrower opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    User,
    Project,
    Local,
}

impl Target {
    pub fn path(self) -> PathBuf {
        match self {
            Target::User => crate::board::home().join(".claude/settings.json"),
            Target::Project => PathBuf::from(".claude/settings.json"),
            Target::Local => PathBuf::from(".claude/settings.local.json"),
        }
    }
}

/// A missing settings file reads as an empty object, so installing into a fresh
/// machine works without ceremony.
pub fn read(path: &Path) -> io::Result<Value> {
    match fs::read_to_string(path) {
        Ok(body) if body.trim().is_empty() => Ok(json!({})),
        Ok(body) => serde_json::from_str(&body)
            .map_err(|e| io::Error::other(format!("{} is not valid JSON: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e),
    }
}

/// The first modification leaves a `.bak` alongside.
pub fn write(path: &Path, rendered: &str) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let backup = path.with_extension("json.bak");
    if path.exists() && !backup.exists() {
        fs::copy(path, &backup)?;
    }
    fs::write(path, rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unrelated keys, an unrelated `PreToolUse` hook, and a deliberately
    /// non-alphabetical key order — all of which must survive.
    const FIXTURE: &str = r#"{
  "theme": "dark",
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "/usr/local/bin/audit-bash"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "say done"
          }
        ]
      }
    ]
  },
  "apiKeyHelper": "~/bin/key"
}
"#;

    const ANDON: &str = "/opt/andon/bin/andon guard";

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture parses")
    }

    #[test]
    fn the_fixture_is_already_in_the_shape_we_write() {
        // The byte-identical promise below holds for files in the canonical
        // two-space pretty form, which is what Claude Code itself writes.
        assert_eq!(render(&parse(FIXTURE)), FIXTURE);
    }

    #[test]
    fn install_leaves_everything_else_alone() {
        let edit = install(&parse(FIXTURE), ANDON);
        assert_eq!(edit.outcome, Outcome::Added);
        let after = parse(&edit.rendered);

        assert_eq!(after["theme"], "dark");
        assert_eq!(after["apiKeyHelper"], "~/bin/key");
        assert!(after["hooks"]["Stop"].is_array(), "other events untouched");

        let groups = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0]["matcher"], "Bash",
            "the foreign hook keeps its place"
        );
        assert_eq!(groups[1]["matcher"], "*");
        assert_eq!(groups[1]["hooks"][0]["command"], ANDON);

        let keys: Vec<&String> = after.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["theme", "hooks", "apiKeyHelper"],
            "ordering survives"
        );
    }

    #[test]
    fn installing_twice_does_not_duplicate() {
        let once = install(&parse(FIXTURE), ANDON);
        let twice = install(&parse(&once.rendered), ANDON);
        assert_eq!(twice.outcome, Outcome::Unchanged);
        assert_eq!(twice.rendered, once.rendered);
    }

    #[test]
    fn installing_from_a_new_location_repoints_the_existing_entry() {
        // Which doubles as the fix for a moved or reinstalled binary.
        let once = install(&parse(FIXTURE), ANDON);
        let moved = install(&parse(&once.rendered), "/opt/homebrew/bin/andon guard");
        assert_eq!(moved.outcome, Outcome::Updated);

        let groups = parse(&moved.rendered)["hooks"]["PreToolUse"].clone();
        let groups = groups.as_array().unwrap();
        assert_eq!(groups.len(), 2, "repointed, not appended");
        assert_eq!(
            groups[1]["hooks"][0]["command"],
            "/opt/homebrew/bin/andon guard"
        );
    }

    #[test]
    fn installing_into_nothing_at_all_works() {
        let edit = install(&serde_json::json!({}), ANDON);
        assert_eq!(edit.outcome, Outcome::Added);
        assert_eq!(
            parse(&edit.rendered)["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            ANDON
        );
    }

    #[test]
    fn uninstall_returns_the_file_to_its_prior_shape() {
        // The real test: it catches an uninstall that leaves empty scaffolding
        // behind, or reorders someone's config on the way through.
        let installed = install(&parse(FIXTURE), ANDON);
        let removed = uninstall(&parse(&installed.rendered));
        assert_eq!(removed.outcome, Outcome::Removed);
        assert_eq!(removed.rendered, FIXTURE);
    }

    #[test]
    fn uninstall_prunes_scaffolding_it_created() {
        let installed = install(&serde_json::json!({ "theme": "dark" }), ANDON);
        let removed = uninstall(&parse(&installed.rendered));
        assert_eq!(removed.rendered, "{\n  \"theme\": \"dark\"\n}\n");
    }

    #[test]
    fn uninstall_never_touches_other_peoples_hooks() {
        let removed = uninstall(&parse(FIXTURE));
        assert_eq!(removed.outcome, Outcome::NotPresent);
        assert_eq!(removed.rendered, FIXTURE, "finding nothing changes nothing");
    }

    #[test]
    fn our_entry_is_recognised_wherever_the_binary_lives() {
        let ours = |command: &str| is_ours(&serde_json::json!({ "command": command }));
        assert!(ours("/opt/andon/bin/andon guard"));
        assert!(ours("andon guard"));
        assert!(ours("\"/Applications/My Tools/andon\" guard"));
        assert!(!ours("/usr/local/bin/audit-bash"));
        assert!(!ours("/opt/andon/bin/andon serve"));
        assert!(!ours("/opt/bin/vanguard"), "guard must be its own argument");
        assert!(!ours("/opt/bin/other guard"));
    }
}
