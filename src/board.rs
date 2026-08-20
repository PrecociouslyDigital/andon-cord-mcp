//! The state directory: where cords live, and the handful of moves that can be
//! made on them.
//!
//! There is no daemon and no lock. A cord has exactly one writer at a time —
//! the server creates it, the CLI answers it — and every write is
//! write-temp-then-`rename`, which is atomic on POSIX. That is the whole
//! concurrency story, and it is sufficient.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::cord::{Cord, is_id_prefix, random_id};

/// `$ANDON_STATE_DIR` → `$XDG_STATE_HOME/andon-cord` → `~/.local/state/andon-cord`.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = env_path("ANDON_STATE_DIR") {
        return dir;
    }
    if let Some(dir) = env_path("XDG_STATE_HOME") {
        return dir.join("andon-cord");
    }
    home().join(".local/state/andon-cord")
}

/// Live cords only. The guard scans this directory on every tool call, so its
/// size must be the number of *currently open* cords — never one that grows
/// with every cord ever pulled.
pub fn cords_dir() -> PathBuf {
    state_dir().join("cords")
}

pub fn archive_dir() -> PathBuf {
    state_dir().join("archive")
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn home() -> PathBuf {
    env_path("HOME").unwrap_or_else(|| PathBuf::from("."))
}

fn cord_path(id: &str) -> PathBuf {
    cords_dir().join(format!("{id}.json"))
}

/// Generate an id, then claim it by creating the file with `O_CREAT|O_EXCL`.
/// The exclusive create *is* the allocation, so two agents pulling at the same
/// moment cannot collide — a collision is a retry, not a bug.
pub fn allocate(session: String, cwd: String, pid: u32, report: Value) -> io::Result<Cord> {
    let dir = cords_dir();
    fs::create_dir_all(&dir)?;
    for _ in 0..64 {
        let id = random_id();
        let path = dir.join(format!("{id}.json"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => {
                let cord = Cord::new(id, session, cwd, pid, report);
                // The claim holds while we rename the real contents over it.
                save(&cord)?;
                return Ok(cord);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("could not allocate a free cord id"))
}

/// Overwrite a cord in place, atomically.
pub fn save(cord: &Cord) -> io::Result<()> {
    let path = cord_path(&cord.id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(cord).map_err(io::Error::other)?;
    write_atomic(&path, &body)
}

fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)
}

pub fn load(id: &str) -> io::Result<Cord> {
    let body = fs::read(cord_path(id))?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

/// Every cord currently in `cords/`, oldest first. Unreadable and malformed
/// files are skipped rather than raised: a corrupt cord must not take out the
/// listing, and above all must not take out the guard.
pub fn live() -> Vec<Cord> {
    let Ok(entries) = fs::read_dir(cords_dir()) else {
        return Vec::new();
    };
    let mut cords: Vec<Cord> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| fs::read(p).ok())
        .filter_map(|body| serde_json::from_slice(&body).ok())
        .collect();
    cords.sort_by_key(|c: &Cord| c.pulled_at);
    cords
}

/// Cords that are stopping the line right now: open, and owned by a process
/// that still exists. This is the guard's only question.
pub fn stopping() -> Vec<Cord> {
    live()
        .into_iter()
        .filter(|c| c.status.is_open() && c.agent_alive())
        .collect()
}

#[derive(Debug)]
pub enum Resolve {
    NotFound,
    Ambiguous(Vec<String>),
}

impl std::fmt::Display for Resolve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Resolve::NotFound => write!(f, "no cord matches"),
            Resolve::Ambiguous(ids) => write!(f, "ambiguous, matches {}", ids.join(", ")),
        }
    }
}

/// Resolve an unambiguous prefix, like a git short hash. Prefix matching is
/// also why the random part leads and no timestamp is encoded in the id: a
/// timestamp prefix would make concurrent ids share their opening characters.
///
/// User input is matched against the live cords rather than turned into a path,
/// which is what keeps `andon respond ../../etc/passwd` uninteresting.
pub fn resolve(prefix: &str) -> Result<Cord, Resolve> {
    let needle = prefix.trim().to_ascii_lowercase();
    if !is_id_prefix(&needle) {
        return Err(Resolve::NotFound);
    }
    let mut matches: Vec<Cord> = live()
        .into_iter()
        .filter(|c| c.id.starts_with(&needle))
        .collect();
    match matches.len() {
        0 => Err(Resolve::NotFound),
        1 => Ok(matches.remove(0)),
        _ => Err(Resolve::Ambiguous(
            matches.into_iter().map(|c| c.id).collect(),
        )),
    }
}

/// Move a resolved cord out of the hot path. Called by the reader that closes
/// the loop, and by `reap` for cords whose reader never came back.
pub fn archive(cord: &Cord, retention: Option<Duration>) -> io::Result<()> {
    let dir = archive_dir();
    fs::create_dir_all(&dir)?;
    let body = serde_json::to_vec_pretty(cord).map_err(io::Error::other)?;
    let dest = dir.join(cord.archive_name());
    write_atomic(&dest, &body)?;
    // Best-effort: the cord is safely in the archive either way.
    let _ = fs::remove_file(cord_path(&cord.id));
    prune(retention);
    Ok(())
}

/// `archive/` is pruned at archive time, which needs no daemon and no cron
/// because the only moment the directory grows is the moment something is
/// written to it.
pub fn prune(retention: Option<Duration>) {
    let Some(retention) = retention else { return };
    let Ok(entries) = fs::read_dir(archive_dir()) else {
        return;
    };
    let cutoff = SystemTime::now() - retention;
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Sweep resolved cords that nobody came back to collect, because their agent
/// is gone. Run by human-invoked commands, never by the guard — a guard that
/// mutates state is a guard that can fail in ways that block tool calls.
///
/// An *open* cord whose agent died is deliberately left alone. It is not
/// resolved, it is a crash, and the person who walked over to answer it needs
/// to see that nothing is listening rather than find that it silently
/// vanished. `andon list` marks it and `andon clear` removes it.
pub fn reap(retention: Option<Duration>) -> usize {
    live()
        .into_iter()
        .filter(|cord| !cord.status.is_open() && !cord.agent_alive())
        .filter(|cord| archive(cord, retention).is_ok())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cord::CordStatus;
    use crate::testing::{Sandbox, arb_value};
    use proptest::prelude::*;
    use serde_json::json;

    fn planted(id: &str) -> Cord {
        let cord = Cord::new(
            id.to_string(),
            "s".into(),
            "/tmp/project".into(),
            std::process::id(),
            json!("blocked"),
        );
        save(&cord).expect("plant");
        cord
    }

    #[test]
    fn allocate_opens_a_cord() {
        let _sandbox = Sandbox::new("board-allocate");
        let cord = allocate("s".into(), "/tmp".into(), 1, json!("help")).unwrap();
        assert_eq!(cord.id.len(), crate::cord::ID_LEN);
        assert!(cord.status.is_open());
        assert_eq!(load(&cord.id).unwrap(), cord);
    }

    #[test]
    fn ids_do_not_collide() {
        let _sandbox = Sandbox::new("board-unique");
        let ids: std::collections::HashSet<String> = (0..200)
            .map(|_| allocate("s".into(), "/tmp".into(), 1, json!(1)).unwrap().id)
            .collect();
        assert_eq!(ids.len(), 200, "the exclusive create is the allocation");
    }

    #[test]
    fn a_unique_prefix_resolves() {
        let _sandbox = Sandbox::new("board-prefix");
        planted("k7f2");
        planted("q3xz");
        assert_eq!(resolve("k7").unwrap().id, "k7f2");
        assert_eq!(
            resolve("K7F2").unwrap().id,
            "k7f2",
            "ids read aloud lose case"
        );
    }

    #[test]
    fn an_ambiguous_prefix_lists_the_candidates_rather_than_picking() {
        let _sandbox = Sandbox::new("board-ambiguous");
        planted("k7f2");
        planted("k7g9");
        let Err(Resolve::Ambiguous(mut ids)) = resolve("k7") else {
            panic!("expected an ambiguous match");
        };
        ids.sort();
        assert_eq!(ids, vec!["k7f2", "k7g9"]);
    }

    #[test]
    fn no_match_is_not_found() {
        let _sandbox = Sandbox::new("board-missing");
        planted("k7f2");
        assert!(matches!(resolve("zz"), Err(Resolve::NotFound)));
        assert!(matches!(
            resolve("../../etc/passwd"),
            Err(Resolve::NotFound)
        ));
        assert!(matches!(resolve(""), Err(Resolve::NotFound)));
    }

    #[test]
    fn archiving_empties_the_hot_path() {
        let _sandbox = Sandbox::new("board-archive");
        let mut cord = planted("k7f2");
        cord.status = CordStatus::Answered {
            guidance: "do X".into(),
            answered_at: crate::cord::now(),
        };
        archive(&cord, None).unwrap();

        assert!(live().is_empty(), "the guard must never scan history");
        let archived: Vec<_> = fs::read_dir(archive_dir()).unwrap().flatten().collect();
        assert_eq!(archived.len(), 1);
        let name = archived[0].file_name().to_string_lossy().to_string();
        assert!(name.ends_with("-k7f2.json"), "got {name}");
        assert!(name.starts_with(char::is_numeric), "timestamped: {name}");
    }

    #[test]
    fn a_stopping_cord_is_open_and_alive() {
        let _sandbox = Sandbox::new("board-stopping");
        planted("k7f2");
        assert_eq!(stopping().len(), 1);

        let mut dead = planted("q3xz");
        // pid 1 is init, which we are certainly not; a never-used high pid is
        // the reliable way to name a process that is gone.
        dead.pid = 0x7FFF_FFFE;
        save(&dead).unwrap();
        assert_eq!(stopping().len(), 1, "a dead agent never blocks");
    }

    #[test]
    fn reaping_collects_answers_the_agent_never_came_back_for() {
        let _sandbox = Sandbox::new("board-reap");
        let mut uncollected = planted("k7f2");
        uncollected.pid = 0x7FFF_FFFE;
        uncollected.status = CordStatus::Answered {
            guidance: "do X".into(),
            answered_at: crate::cord::now(),
        };
        save(&uncollected).unwrap();
        planted("q3xz");

        assert_eq!(reap(None), 1);
        assert_eq!(live().len(), 1, "the live agent's cord stays");
        assert!(archive_dir().join(uncollected.archive_name()).exists());
    }

    #[test]
    fn a_crashed_agents_open_cord_is_left_for_the_human_to_see() {
        let _sandbox = Sandbox::new("board-reap-crash");
        let mut crashed = planted("k7f2");
        crashed.pid = 0x7FFF_FFFE;
        save(&crashed).unwrap();

        assert_eq!(reap(None), 0, "a crash is not a resolution");
        assert_eq!(live().len(), 1, "it must not silently vanish");
        assert!(stopping().is_empty(), "but it stops blocking anything");
    }

    #[test]
    fn pruning_only_touches_what_is_past_retention() {
        let _sandbox = Sandbox::new("board-prune");
        let mut cord = planted("k7f2");
        cord.status = CordStatus::Cleared {
            reason: "done".into(),
            cleared_at: crate::cord::now(),
        };
        archive(&cord, Some(Duration::from_secs(3600))).unwrap();
        assert_eq!(fs::read_dir(archive_dir()).unwrap().count(), 1);

        // Nothing written a moment ago is a month old.
        prune(Some(Duration::from_secs(0)));
        assert_eq!(fs::read_dir(archive_dir()).unwrap().count(), 0);
    }

    #[test]
    fn a_corrupt_cord_does_not_take_out_the_listing() {
        let _sandbox = Sandbox::new("board-corrupt");
        planted("k7f2");
        fs::write(cords_dir().join("junk.json"), b"{not json").unwrap();
        assert_eq!(live().len(), 1);
    }

    proptest! {
        /// The invariant whose violation would silently push agents back toward
        /// working around us: whatever the report is, the cord opens.
        #[test]
        fn any_report_at_all_opens_a_cord(report in arb_value()) {
            let _sandbox = Sandbox::new("board-any-report");
            let cord = allocate("s".into(), "/tmp".into(), 1, report.clone()).unwrap();
            prop_assert!(cord.status.is_open());
            prop_assert_eq!(load(&cord.id).unwrap().report, report);
        }
    }
}
