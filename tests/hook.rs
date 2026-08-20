//! Installing and removing the guard hook from a real settings file.

mod support;

use serde_json::Value;
use support::{BIN, Sandbox};

/// Unrelated keys, an unrelated `PreToolUse` hook, and a key order that is not
/// alphabetical — all of which must come back untouched.
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
    ]
  },
  "apiKeyHelper": "~/bin/key"
}
"#;

fn settings_path(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox.dir.join(".claude/settings.json")
}

fn seed(name: &str) -> Sandbox {
    let sandbox = Sandbox::new(name);
    std::fs::create_dir_all(sandbox.dir.join(".claude")).unwrap();
    std::fs::write(settings_path(&sandbox), FIXTURE).unwrap();
    sandbox
}

fn read(sandbox: &Sandbox) -> String {
    std::fs::read_to_string(settings_path(sandbox)).unwrap()
}

#[test]
fn install_then_uninstall_leaves_the_file_exactly_as_it_was() {
    let sandbox = seed("hook-roundtrip");

    let out = sandbox.andon(&["install-hook", "--project"]);
    assert!(out.status.success(), "{out:?}");
    let installed: Value = serde_json::from_str(&read(&sandbox)).unwrap();

    // The absolute path, not the bare name, so the hook keeps working when
    // Claude Code is launched with a PATH that lacks the install directory.
    let groups = installed["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups[0]["matcher"], "Bash",
        "the foreign hook is untouched"
    );
    assert_eq!(groups[1]["matcher"], "*");
    let command = groups[1]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.ends_with(" guard"), "{command}");
    assert!(
        command.starts_with('/') || command.starts_with('"'),
        "{command}"
    );

    let keys: Vec<&String> = installed.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec!["theme", "hooks", "apiKeyHelper"],
        "ordering survives"
    );

    // The first modification leaves a backup alongside.
    let backup = sandbox.dir.join(".claude/settings.json.bak");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), FIXTURE);

    // Installing twice must not append a duplicate.
    assert!(
        sandbox
            .andon(&["install-hook", "--project"])
            .status
            .success()
    );
    let twice: Value = serde_json::from_str(&read(&sandbox)).unwrap();
    assert_eq!(twice["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);

    // The real assertion: an uninstall that leaves empty scaffolding behind, or
    // reorders someone's config, fails right here.
    let out = sandbox.andon(&["uninstall-hook", "--project"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(read(&sandbox), FIXTURE);
}

#[test]
fn a_dry_run_shows_the_result_without_writing_it() {
    let sandbox = seed("hook-dry");
    let out = sandbox.andon(&["install-hook", "--project", "--dry-run"]);
    assert!(out.status.success());

    let shown: Value = serde_json::from_slice(&out.stdout).expect("valid JSON on stdout");
    assert_eq!(shown["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    assert_eq!(read(&sandbox), FIXTURE, "nothing was written");
    assert!(!sandbox.dir.join(".claude/settings.json.bak").exists());
}

#[test]
fn installing_into_a_machine_with_no_settings_file_works() {
    let sandbox = Sandbox::new("hook-fresh");
    assert!(sandbox.andon(&["install-hook", "--local"]).status.success());

    let body = std::fs::read_to_string(sandbox.dir.join(".claude/settings.local.json")).unwrap();
    let settings: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "*");
}

#[test]
fn finding_nothing_to_remove_says_so_and_succeeds() {
    let sandbox = seed("hook-absent");
    let out = sandbox.andon(&["uninstall-hook", "--project"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("No andon guard hook"));
    assert_eq!(read(&sandbox), FIXTURE);
}

#[test]
fn a_settings_file_that_is_not_json_is_refused_rather_than_overwritten() {
    let sandbox = Sandbox::new("hook-broken");
    std::fs::create_dir_all(sandbox.dir.join(".claude")).unwrap();
    std::fs::write(settings_path(&sandbox), "{ not json").unwrap();

    let out = sandbox.andon(&["install-hook", "--project"]);
    assert!(!out.status.success());
    assert_eq!(
        read(&sandbox),
        "{ not json",
        "someone's file is not ours to reset"
    );
}

#[test]
fn the_installed_command_is_one_the_shell_can_actually_run() {
    let sandbox = seed("hook-runnable");
    sandbox.andon(&["install-hook", "--project"]);
    let settings: Value = serde_json::from_str(&read(&sandbox)).unwrap();
    let command = settings["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(command, format!("{BIN} guard"));

    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("echo '{{}}' | {command}"))
        .output()
        .expect("run the hook the way a shell would");
    assert_eq!(out.status.code(), Some(0), "an empty payload allows");
}
