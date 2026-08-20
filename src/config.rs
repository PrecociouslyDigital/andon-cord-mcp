//! Configuration, and the one rule that governs it: bad config degrades, never
//! blocks. A tool whose job is to be pullable at the worst possible moment must
//! not be the thing that refuses to start.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Deserializer};

use crate::notify::Notifier;

/// The description is the entire product. With the schema deliberately doing
/// nothing, this text is what decides whether a stuck agent reaches for the
/// cord or for a workaround.
pub const DEFAULT_DESCRIPTION: &str = "\
If you are facing an obstacle you can't resolve, and are getting desperate, or \
too creative: pull this andon cord. It stops the current workstream and summons \
a human. Common reasons to do so are: a missing credential, a captcha or other \
human-gate, or crucial private context you haven't been explicitly told to fetch.

Pulling the cord is always a valid and welcome outcome; it is never a failure, \
and it is strongly preferred over any overly creative workaround.

Say whatever you need to, in whatever shape you like — a single sentence is \
fine. If you have more to give, the most useful things to include are what you \
were trying to do, what is blocking you, and what you would need in order to \
continue.

This call blocks until a human answers, and returns their guidance.";

/// Which sessions a stopped line stops. Session scope is the default precisely
/// so a cord in one repo doesn't freeze an unrelated session in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Session,
    Project,
    Global,
}

impl std::str::FromStr for Scope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "session" => Ok(Scope::Session),
            "project" => Ok(Scope::Project),
            "global" => Ok(Scope::Global),
            other => Err(format!("unknown scope {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub scope: Scope,
    pub guard: bool,
    pub elicit: bool,
    /// How long a single blocking call holds open before handing back an
    /// `await_cord` invitation.
    #[serde(deserialize_with = "de_duration")]
    pub max_wait: Duration,
    /// How many times the agent may re-enter before the cord is abandoned.
    /// Bounds worst-case token spend to a handful of turns.
    pub max_reentries: u32,
    /// `None` is `"forever"`.
    #[serde(deserialize_with = "de_retention")]
    pub retention: Option<Duration>,
    pub description: Option<String>,
    pub description_file: Option<PathBuf>,
    pub description_append: Option<String>,
    pub notify: Vec<Notifier>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            scope: Scope::default(),
            guard: true,
            elicit: false,
            max_wait: Duration::from_secs(60 * 60),
            max_reentries: 3,
            retention: Some(Duration::from_secs(30 * 24 * 60 * 60)),
            description: None,
            description_file: None,
            description_append: None,
            // Summoning is the whole job, so the default has to actually reach
            // someone. The bell is free and instant where a terminal is in
            // view; the desktop notification is what survives a full-screen
            // TUI, a backgrounded window, and a server with no terminal of its
            // own. Both are one config line to remove.
            notify: vec![Notifier::Bell, Notifier::desktop()],
        }
    }
}

impl Config {
    /// Reads the config file if there is one, then applies environment
    /// overrides. Every failure along the way falls back to a default and says
    /// so on stderr: a broken config that silently disables the cord is
    /// strictly worse than one that ignores itself loudly.
    pub fn load() -> Config {
        let mut config = match Self::read_file() {
            Ok(config) => config,
            Err(msg) => {
                if let Some(msg) = msg {
                    eprintln!("andon: {msg}; using defaults");
                }
                Config::default()
            }
        };
        config.apply_env();
        config
    }

    /// `Err(None)` means there simply is no config file, which is the common
    /// case and not worth mentioning.
    fn read_file() -> Result<Config, Option<String>> {
        let path = Self::path();
        let body = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => None,
            _ => Some(format!("could not read {}: {e}", path.display())),
        })?;
        serde_json::from_str(&body)
            .map_err(|e| Some(format!("could not parse {}: {e}", path.display())))
    }

    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("ANDON_CONFIG").filter(|v| !v.is_empty()) {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::board::home().join(".config"));
        base.join("andon-cord/config.json")
    }

    /// The common case needs no config file at all.
    fn apply_env(&mut self) {
        if let Some(raw) = env("ANDON_SCOPE") {
            match raw.parse() {
                Ok(scope) => self.scope = scope,
                Err(e) => eprintln!("andon: ANDON_SCOPE ignored: {e}"),
            }
        }
        if let Some(on) = env("ANDON_GUARD").map(|v| truthy(&v)) {
            self.guard = on;
        }
        if let Some(on) = env("ANDON_ELICIT").map(|v| truthy(&v)) {
            self.elicit = on;
        }
        if let Some(url) = env("ANDON_WEBHOOK_URL") {
            self.notify.push(Notifier::webhook(url));
        }
    }

    /// `description_file` > `description` > the compiled-in default, and then
    /// whatever that resolved to takes `description_append`.
    ///
    /// Read fresh on every `tools/list` rather than cached at startup, so it
    /// costs nothing to keep current.
    pub fn description(&self) -> String {
        let from_file = self.description_file.as_ref().and_then(|path| {
            std::fs::read_to_string(path)
                .map_err(|e| eprintln!("andon: description_file {}: {e}", path.display()))
                .ok()
        });
        let base = from_file
            .or_else(|| self.description.clone())
            .unwrap_or_else(|| DEFAULT_DESCRIPTION.to_string());
        match self.description_append.as_deref().map(str::trim) {
            Some(extra) if !extra.is_empty() => format!("{}\n\n{extra}", base.trim_end()),
            _ => base,
        }
    }

    /// Each re-entry buys progressively more wall clock, so a handful of turns
    /// covers a long absence.
    pub fn backoff(&self, reentries: u32) -> Duration {
        self.max_wait * 2u32.saturating_pow(reentries.min(16))
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn truthy(v: &str) -> bool {
    !matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// `90s`, `5m`, `1h`, `30d`, or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (digits, unit) = s.split_at(s.trim_end_matches(char::is_alphabetic).len());
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("expected a duration like \"30m\" or \"2h\", got {s:?}"))?;
    let secs = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 60 * 60,
        "d" | "day" | "days" => 24 * 60 * 60,
        other => return Err(format!("unknown duration unit {other:?} in {s:?}")),
    };
    Ok(Duration::from_secs(n.saturating_mul(secs)))
}

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let raw = String::deserialize(d)?;
    parse_duration(&raw).map_err(serde::de::Error::custom)
}

fn de_retention<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    let raw = String::deserialize(d)?;
    if matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "forever" | "never"
    ) {
        return Ok(None);
    }
    parse_duration(&raw)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sandbox;

    #[test]
    fn no_config_file_is_the_common_case() {
        let sandbox = Sandbox::new("config-none");
        sandbox.set("ANDON_CONFIG", sandbox.dir.join("nowhere.json"));
        let config = Config::load();
        assert_eq!(config.scope, Scope::Session);
        assert!(config.guard);
        assert!(!config.elicit);
        assert_eq!(config.description(), DEFAULT_DESCRIPTION);
    }

    #[test]
    fn malformed_config_degrades_to_defaults() {
        let sandbox = Sandbox::new("config-broken");
        sandbox.config("{ this is not json");
        let config = Config::load();
        assert!(config.guard, "a broken config must not disable the cord");
        assert_eq!(config.max_reentries, Config::default().max_reentries);
    }

    #[test]
    fn file_values_are_read() {
        let sandbox = Sandbox::new("config-read");
        sandbox.config(
            r#"{ "scope": "project", "guard": false, "elicit": true,
                 "max_wait": "90s", "max_reentries": 7, "retention": "forever" }"#,
        );
        let config = Config::load();
        assert_eq!(config.scope, Scope::Project);
        assert!(!config.guard);
        assert!(config.elicit);
        assert_eq!(config.max_wait, Duration::from_secs(90));
        assert_eq!(config.max_reentries, 7);
        assert_eq!(config.retention, None);
    }

    #[test]
    fn env_overrides_the_file() {
        let sandbox = Sandbox::new("config-env");
        sandbox.config(r#"{ "scope": "global", "guard": true }"#);
        sandbox.set("ANDON_SCOPE", "project");
        sandbox.set("ANDON_GUARD", "0");
        let config = Config::load();
        assert_eq!(config.scope, Scope::Project);
        assert!(!config.guard, "ANDON_GUARD=0 is the off switch");
    }

    #[test]
    fn webhook_url_needs_no_config_file() {
        let sandbox = Sandbox::new("config-webhook");
        sandbox.set("ANDON_CONFIG", sandbox.dir.join("nowhere.json"));
        sandbox.set("ANDON_WEBHOOK_URL", "https://example.invalid/hook");
        let notifiers = Config::load().notify;
        assert!(
            matches!(
                notifiers.last(),
                Some(crate::notify::Notifier::Webhook { .. })
            ),
            "the defaults stay; the webhook is added alongside them: {notifiers:?}"
        );
    }

    #[test]
    fn description_precedence_is_file_then_inline_then_default() {
        let sandbox = Sandbox::new("config-desc");
        let file = sandbox.write("nudge.md", "from the file");

        let inline = Config {
            description: Some("inline".into()),
            ..Config::default()
        };
        assert_eq!(inline.description(), "inline");

        let from_file = Config {
            description_file: Some(file),
            ..inline.clone()
        };
        assert_eq!(from_file.description(), "from the file");
    }

    #[test]
    fn a_missing_description_file_falls_back_rather_than_failing() {
        let sandbox = Sandbox::new("config-desc-missing");
        let config = Config {
            description_file: Some(sandbox.dir.join("gone.md")),
            description: Some("inline".into()),
            ..Config::default()
        };
        assert_eq!(config.description(), "inline");

        let bare = Config {
            description_file: Some(sandbox.dir.join("gone.md")),
            ..Config::default()
        };
        assert_eq!(bare.description(), DEFAULT_DESCRIPTION);
    }

    #[test]
    fn append_applies_to_whatever_resolved() {
        let sandbox = Sandbox::new("config-append");
        let file = sandbox.write("nudge.md", "from the file\n");
        let append = Some("Say which ticket you are on.".to_string());

        let on_default = Config {
            description_append: append.clone(),
            ..Config::default()
        };
        assert!(
            on_default
                .description()
                .starts_with("If you are facing an obstacle")
        );
        assert!(
            on_default
                .description()
                .ends_with("Say which ticket you are on.")
        );

        let on_file = Config {
            description_file: Some(file),
            description_append: append,
            ..Config::default()
        };
        assert_eq!(
            on_file.description(),
            "from the file\n\nSay which ticket you are on."
        );
    }

    #[test]
    fn the_default_summons_does_not_depend_on_anyone_watching_a_terminal() {
        // A cord nobody notices is a cord nobody answers, which is the only way
        // this tool can fail completely and silently.
        let notify = Config::default().notify;
        assert!(
            notify
                .iter()
                .any(|n| matches!(n, crate::notify::Notifier::Desktop { .. })),
            "the shipped default must reach past the terminal: {notify:?}"
        );
    }

    #[test]
    fn the_default_description_keeps_the_part_most_easily_lost() {
        // The framing is the product. A house-style edit that drops it is the
        // failure this test exists to notice.
        assert!(DEFAULT_DESCRIPTION.contains("never a failure"));
        assert!(DEFAULT_DESCRIPTION.contains("blocks until a human answers"));
    }

    #[test]
    fn durations_parse_the_way_people_write_them() {
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(
            parse_duration(" 30d ").unwrap(),
            Duration::from_secs(2_592_000)
        );
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("3 fortnights").is_err());
    }

    #[test]
    fn backoff_buys_more_wall_clock_each_time() {
        let config = Config {
            max_wait: Duration::from_secs(60),
            ..Config::default()
        };
        assert_eq!(config.backoff(0), Duration::from_secs(60));
        assert_eq!(config.backoff(1), Duration::from_secs(120));
        assert_eq!(config.backoff(3), Duration::from_secs(480));
    }
}
