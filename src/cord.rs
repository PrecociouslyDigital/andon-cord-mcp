//! What a pulled cord is, and how it reads.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A cord is created `Open` and resolves exactly once. The guidance exists
/// precisely when someone has answered; no other combination is representable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CordStatus {
    Open,
    Answered {
        guidance: String,
        #[serde(with = "epoch_millis")]
        answered_at: SystemTime,
    },
    /// The agent stopped waiting. Nothing is listening for a reply any more.
    Abandoned {
        #[serde(with = "duration_millis")]
        waited: Duration,
        #[serde(with = "epoch_millis")]
        at: SystemTime,
    },
    Cleared {
        reason: String,
        #[serde(with = "epoch_millis")]
        cleared_at: SystemTime,
    },
}

impl CordStatus {
    pub fn is_open(&self) -> bool {
        matches!(self, CordStatus::Open)
    }

    /// The one-word label `andon list` prints.
    pub fn label(&self) -> &'static str {
        match self {
            CordStatus::Open => "open",
            CordStatus::Answered { .. } => "answered",
            CordStatus::Abandoned { .. } => "abandoned",
            CordStatus::Cleared { .. } => "cleared",
        }
    }
}

/// `pulled_at` sits on the cord rather than inside `Open`, because a cord was
/// pulled at some moment whether or not it is still open — the archive filename
/// needs that moment long after the status has moved on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cord {
    pub id: String,
    pub session: String,
    pub cwd: String,
    pub pid: u32,
    #[serde(with = "epoch_millis")]
    pub pulled_at: SystemTime,
    /// Whatever the agent sent, verbatim. Never validated, never reshaped.
    pub report: Value,
    pub status: CordStatus,
    /// How many times the agent has re-entered `await_cord` for this cord.
    #[serde(default)]
    pub reentries: u32,
}

impl Cord {
    pub fn new(id: String, session: String, cwd: String, pid: u32, report: Value) -> Self {
        Cord {
            id,
            session,
            cwd,
            pid,
            pulled_at: now(),
            report,
            status: CordStatus::Open,
            reentries: 0,
        }
    }

    /// False once the agent that pulled this cord is gone. A dead cord never
    /// blocks anything: without this, a crash mid-cord would wedge the guard
    /// forever.
    pub fn agent_alive(&self) -> bool {
        pid_alive(self.pid)
    }

    pub fn age(&self) -> Duration {
        self.pulled_at.elapsed().unwrap_or_default()
    }

    /// `<pulled_at>-<id>.json`, zero-padded so `archive/` sorts chronologically
    /// and two same-id cords from different days are simply different files.
    pub fn archive_name(&self) -> String {
        format!("{:013}-{}.json", to_millis(self.pulled_at), self.id)
    }
}

/// Crockford base32, lowercased: digits plus consonant-heavy letters with `i`,
/// `l`, `o` and `u` dropped, so nothing is misread off a phone screen and no
/// accidental words appear.
const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Four characters. The id's primary consumer is a human retyping it into a
/// terminal, and uniqueness rests on the exclusive create in [`crate::board`]
/// rather than on 32^4 being a large number.
pub const ID_LEN: usize = 4;

pub fn random_id() -> String {
    random_bytes(ID_LEN)
        .into_iter()
        .map(|b| ALPHABET[usize::from(b) % ALPHABET.len()] as char)
        .collect()
}

/// True for a string that could be an id or the leading part of one. User input
/// never becomes a path directly — it is matched against the live cords — but
/// rejecting junk early makes the error messages better.
pub fn is_id_prefix(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= ID_LEN
        && s.bytes()
            .all(|b| ALPHABET.contains(&b.to_ascii_lowercase()))
}

/// Reading `/dev/urandom` directly, consistent with the POSIX assumptions
/// already baked in via `kill(pid, 0)`, `/dev/tty` and atomic `rename`. Falls
/// back to clock and pid entropy rather than failing: a slightly worse id is
/// enormously better than a cord that could not be pulled.
fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom")
        && f.read_exact(&mut buf).is_ok()
    {
        return buf;
    }
    let mut seed = to_millis(SystemTime::now()) ^ (u64::from(std::process::id()) << 32);
    for slot in buf.iter_mut() {
        // xorshift64*, enough to spread a timestamp across four characters.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *slot = (seed >> 24) as u8;
    }
    buf
}

/// `kill(pid, 0)` tests for the existence of a process without signalling it.
/// Declared here rather than pulled in with the `libc` crate: it is three lines,
/// and the guard's dependency tree is part of its budget.
pub fn pid_alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // Errors are EPERM (alive, not ours) or ESRCH (gone); only ESRCH means dead,
    // and treating EPERM as alive is the fail-safe direction for the guard.
    let rc = unsafe { kill(pid as i32, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

const ESRCH: i32 = 3;

/// Renders a report for a human: a bare string prints as prose, an object as
/// key/value lines. Whatever an agent sent has to read as *something*.
pub fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}: {}", inline(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Array(items) => items.iter().map(render).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

/// One line's worth of a value, for the right-hand side of a key.
fn inline(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(_) | Value::Object(_) => v.to_string(),
        other => other.to_string(),
    }
}

/// Cuts to `max` characters on a character boundary, marking the cut. Notifiers
/// have length limits; reports do not.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// Now, truncated to the precision a cord record can actually hold. Using this
/// everywhere keeps an in-memory cord equal to the one that comes back off
/// disk, rather than equal-except-for-nanoseconds-nobody-stored.
pub fn now() -> SystemTime {
    from_millis(to_millis(SystemTime::now()))
}

pub fn to_millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub fn from_millis(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// Cord records are read with `jq` and by eye at least as often as by us, so
/// timestamps go on the wire as plain millisecond numbers rather than as
/// serde's default two-field `SystemTime`.
mod epoch_millis {
    use std::time::SystemTime;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(super::to_millis(*t))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        Ok(super::from_millis(u64::deserialize(d)?))
    }
}

mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis().min(u128::from(u64::MAX)) as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

/// ISO-8601 in UTC. Notifications land on phones in other timezones, and a
/// notification reading `pulled_at 1755701234567` helps nobody.
pub fn format_utc(t: SystemTime) -> String {
    let secs = to_millis(t) / 1000;
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod / 60) % 60,
        tod % 60
    )
}

/// Hinnant's `civil_from_days`: a calendar date from a count of days since the
/// epoch, with no lookup tables and no dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe as i64 + era * 400 + i64::from(month <= 2), month, day)
}

/// A rough span of time, in the largest unit that still says something: "4m",
/// "2h", "3d".
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The same span, as a moment in the past. "4m ago" reads better than a
/// timestamp when the question is whether anyone is still waiting.
pub fn format_ago(d: Duration) -> String {
    format!("{} ago", format_duration(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::arb_value;
    use proptest::prelude::*;
    use serde_json::json;

    fn arb_status() -> impl Strategy<Value = CordStatus> {
        prop_oneof![
            Just(CordStatus::Open),
            (".*", 0u64..2_000_000_000_000).prop_map(|(guidance, at)| CordStatus::Answered {
                guidance,
                answered_at: from_millis(at),
            }),
            (0u64..100_000_000, 0u64..2_000_000_000_000).prop_map(|(waited, at)| {
                CordStatus::Abandoned {
                    waited: Duration::from_millis(waited),
                    at: from_millis(at),
                }
            }),
            (".*", 0u64..2_000_000_000_000).prop_map(|(reason, at)| CordStatus::Cleared {
                reason,
                cleared_at: from_millis(at),
            }),
        ]
    }

    #[test]
    fn a_cord_reads_as_plain_json() {
        let mut cord = Cord::new("k7f2".into(), "s".into(), "/repo".into(), 7, json!("help"));
        cord.pulled_at = from_millis(1_700_000_000_000);
        let value: serde_json::Value = serde_json::to_value(&cord).unwrap();
        assert_eq!(value["pulled_at"], json!(1_700_000_000_000u64));
        assert_eq!(value["status"], json!({ "type": "open" }));
    }

    #[test]
    fn guidance_exists_exactly_when_a_cord_has_been_answered() {
        // The state machine, such as it is: no other combination is
        // representable, so there is nothing to test but the shape.
        let answered = CordStatus::Answered {
            guidance: "do X".into(),
            answered_at: from_millis(0),
        };
        assert!(!answered.is_open());
        assert_eq!(answered.label(), "answered");
        assert!(CordStatus::Open.is_open());
    }

    #[test]
    fn ids_avoid_the_characters_that_get_misread() {
        for _ in 0..500 {
            let id = random_id();
            assert_eq!(id.len(), ID_LEN);
            assert!(
                !id.contains(['i', 'l', 'o', 'u']),
                "{id} would be misread off a phone"
            );
            assert!(is_id_prefix(&id));
        }
    }

    #[test]
    fn only_plausible_ids_are_prefixes() {
        assert!(is_id_prefix("k"));
        assert!(is_id_prefix("K7F2"));
        assert!(!is_id_prefix(""));
        assert!(!is_id_prefix("k7f2x"), "longer than an id");
        assert!(!is_id_prefix("../.."));
        assert!(!is_id_prefix("k.2"));
    }

    #[test]
    fn a_report_renders_as_something_whatever_it_is() {
        assert_eq!(render(&json!("stuck")), "stuck");
        assert_eq!(render(&json!({"a": 1, "b": "two"})), "a: 1\nb: two");
        assert_eq!(render(&json!(["one", "two"])), "one\ntwo");
        assert_eq!(render(&json!({})), "");
        assert_eq!(render(&json!(null)), "");
        assert_eq!(render(&json!(42)), "42");
    }

    #[test]
    fn truncation_lands_on_character_boundaries() {
        assert_eq!(truncate("héllo wörld", 100), "héllo wörld");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn dates_come_out_of_the_epoch_correctly() {
        assert_eq!(format_utc(from_millis(0)), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_utc(from_millis(1_700_000_000_000)),
            "2023-11-14T22:13:20Z"
        );
        // A leap day, which is where hand-rolled calendars go wrong.
        assert_eq!(
            format_utc(from_millis(1_709_164_800_000)),
            "2024-02-29T00:00:00Z"
        );
    }

    #[test]
    fn ages_read_the_way_someone_would_say_them() {
        assert_eq!(format_ago(Duration::from_secs(5)), "5s ago");
        assert_eq!(format_ago(Duration::from_secs(300)), "5m ago");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
        assert_eq!(format_duration(Duration::from_secs(200_000)), "2d");
    }

    #[test]
    fn this_process_is_alive_and_a_free_pid_is_not() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0x7FFF_FFFE));
    }

    proptest! {
        #[test]
        fn a_cord_round_trips_through_its_file(
            report in arb_value(),
            status in arb_status(),
            pulled in 0u64..2_000_000_000_000,
            reentries in 0u32..10,
        ) {
            let cord = Cord {
                id: "k7f2".into(),
                session: "sess".into(),
                cwd: "/repo".into(),
                pid: 7,
                pulled_at: from_millis(pulled),
                report,
                status,
                reentries,
            };
            let body = serde_json::to_vec_pretty(&cord).unwrap();
            prop_assert_eq!(serde_json::from_slice::<Cord>(&body).unwrap(), cord);
        }

        /// Rendering is total: there is no report that renders to a panic.
        #[test]
        fn any_report_renders(report in arb_value()) {
            let _ = render(&report);
        }
    }
}
