//! The human side of the loop.

use std::io::Read;
use std::process::ExitCode;

use crate::board::{self, Resolve};
use crate::config::Config;
use crate::cord::{
    Cord, CordStatus, format_ago, format_duration, format_utc, now, render, truncate,
};
use crate::guard::{self, Decision, Query};
use crate::settings::{self, Outcome, Target};

const OK: ExitCode = ExitCode::SUCCESS;
const FAILED: ExitCode = ExitCode::FAILURE;
/// The same code the guard denies with, so `check` and `guard` cannot disagree
/// about more than the shape of their output.
const STOPPED: u8 = 2;

pub fn list() -> ExitCode {
    let config = Config::load();
    board::reap(config.retention);
    let cords = board::live();
    if cords.is_empty() {
        println!("No open cords. The line is running.");
        return OK;
    }
    for cord in &cords {
        let note = match &cord.status {
            // The one thing a human most needs to know: nobody is listening.
            CordStatus::Abandoned { .. } => "  (agent gave up; a reply cannot reach it)",
            CordStatus::Open if !cord.agent_alive() => "  (agent process is gone)",
            _ => "",
        };
        println!(
            "{}  {:<9}  {:>7}  {:<14}  {}{note}",
            cord.id,
            cord.status.label(),
            format_ago(cord.age()),
            truncate(&project(cord), 14),
            summary(cord)
        );
    }
    println!("\nAnswer one with: andon respond <id> \"...\"");
    OK
}

/// One line of report, for a listing: the whole thing flattened, so an object
/// report shows more than whichever key happened to come first.
fn summary(cord: &Cord) -> String {
    let rendered = render(&cord.report);
    let flat: Vec<&str> = rendered
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match flat.is_empty() {
        true => "(no report)".to_string(),
        false => truncate(&flat.join(" · "), 88),
    }
}

/// Which project a cord came from, in the space a listing has: usually the repo
/// name, which is what someone with several sessions open needs to see.
fn project(cord: &Cord) -> String {
    std::path::Path::new(&cord.cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cord.cwd.clone())
}

pub fn show(prefix: &str) -> ExitCode {
    let config = Config::load();
    board::reap(config.retention);
    let cord = match board::resolve(prefix) {
        Ok(cord) => cord,
        Err(e) => return not_found(prefix, e),
    };
    println!("cord     {}", cord.id);
    println!("status   {}", cord.status.label());
    println!(
        "pulled   {} ({})",
        format_utc(cord.pulled_at),
        format_ago(cord.age())
    );
    println!("cwd      {}", cord.cwd);
    println!("session  {}", cord.session);
    println!(
        "agent    pid {} ({})",
        cord.pid,
        if cord.agent_alive() { "alive" } else { "gone" }
    );
    if cord.reentries > 0 {
        println!("waited   {} re-entries", cord.reentries);
    }
    println!("\n{}", render(&cord.report));
    if let CordStatus::Answered { guidance, .. } = &cord.status {
        println!("\nanswered with:\n{guidance}");
    }
    OK
}

/// Writes the answer **in place**, in `cords/`. It does not archive: the
/// waiting agent is polling that exact path, and pulling the file out from
/// under it would turn a successful handoff into an `ENOENT` to be handled.
pub fn respond(prefix: &str, guidance: Option<String>) -> ExitCode {
    let config = Config::load();
    board::reap(config.retention);
    let mut cord = match board::resolve(prefix) {
        Ok(cord) => cord,
        Err(e) => return not_found(prefix, e),
    };

    // Warn before the typing, not after. Composing a careful answer into a cord
    // that nothing is waiting on is a small, specific, avoidable misery.
    match &cord.status {
        CordStatus::Abandoned { waited, .. } => eprintln!(
            "warning: cord {} was abandoned after {} — the agent stopped waiting, so this \
             reply will not reach it.",
            cord.id,
            format_duration(*waited)
        ),
        CordStatus::Cleared { reason, .. } => {
            eprintln!("warning: cord {} was already cleared: {reason}", cord.id)
        }
        CordStatus::Answered { .. } => {
            eprintln!(
                "warning: cord {} was already answered; replacing that answer.",
                cord.id
            )
        }
        CordStatus::Open if !cord.agent_alive() => {
            eprintln!("warning: the agent that pulled cord {} is gone.", cord.id)
        }
        CordStatus::Open => {}
    }

    let guidance = match guidance {
        Some(g) => g,
        None => match read_stdin() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("andon: could not read guidance from stdin: {e}");
                return FAILED;
            }
        },
    };
    if guidance.trim().is_empty() {
        eprintln!("andon: refusing to answer cord {} with nothing.", cord.id);
        return FAILED;
    }

    cord.status = CordStatus::Answered {
        guidance: guidance.trim().to_string(),
        answered_at: now(),
    };
    if let Err(e) = board::save(&cord) {
        eprintln!("andon: could not answer cord {}: {e}", cord.id);
        return FAILED;
    }

    // Echoing the report makes answering a recycled id immediately visible
    // rather than silent — ids are unique among live cords, not forever.
    println!(
        "Answered cord {} ({}, in {}):",
        cord.id,
        format_ago(cord.age()),
        cord.cwd
    );
    println!("\n{}\n", indent(&render(&cord.report)));
    println!("with:\n\n{}", indent(guidance.trim()));
    OK
}

pub fn clear(prefix: &str, reason: Option<String>) -> ExitCode {
    let config = Config::load();
    board::reap(config.retention);
    let mut cord = match board::resolve(prefix) {
        Ok(cord) => cord,
        Err(e) => return not_found(prefix, e),
    };
    cord.status = CordStatus::Cleared {
        reason: reason.unwrap_or_else(|| "cleared by a human".to_string()),
        cleared_at: now(),
    };
    if let Err(e) = board::save(&cord) {
        eprintln!("andon: could not clear cord {}: {e}", cord.id);
        return FAILED;
    }
    println!("Cleared cord {}. The line is running again.", cord.id);
    OK
}

/// The harness-neutral primitive. Any harness able to run a command before a
/// tool call can use this; `andon guard` is the same decision wearing Claude
/// Code's clothes.
pub fn check(session: Option<String>, cwd: Option<String>, tool: Option<String>) -> ExitCode {
    let query = Query {
        tool_name: tool,
        session: Some(session.unwrap_or_else(crate::session::id)),
        cwd: Some(cwd.unwrap_or_else(crate::session::cwd)),
    };
    match guard::decide(&query, &Config::load()) {
        Decision::Allow => {
            eprintln!("The line is running.");
            OK
        }
        Decision::Deny { reason, .. } => {
            eprintln!("{reason}");
            ExitCode::from(STOPPED)
        }
    }
}

/// Prints the effective description after merging, so the nudge agents actually
/// see never has to be inferred from the config.
pub fn description() -> ExitCode {
    println!("{}", Config::load().description());
    OK
}

pub fn install_hook(target: Target, dry_run: bool) -> ExitCode {
    let Some(command) = guard_command() else {
        return FAILED;
    };
    let path = target.path();
    let current = match settings::read(&path) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("andon: {e}");
            return FAILED;
        }
    };
    let edit = settings::install(&current, &command);
    if dry_run {
        print!("{}", edit.rendered);
        return OK;
    }
    if edit.outcome.changed()
        && let Err(e) = settings::write(&path, &edit.rendered)
    {
        eprintln!("andon: could not write {}: {e}", path.display());
        return FAILED;
    }
    match edit.outcome {
        Outcome::Added => println!("Installed the guard hook in {}.", path.display()),
        Outcome::Updated => println!("Updated the guard hook in {} to {command}.", path.display()),
        _ => println!(
            "The guard hook was already installed in {}.",
            path.display()
        ),
    }
    println!("Turn it off without editing anything: ANDON_GUARD=0, or \"guard\": false in config.");
    OK
}

pub fn uninstall_hook(target: Target, dry_run: bool) -> ExitCode {
    let path = target.path();
    let current = match settings::read(&path) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("andon: {e}");
            return FAILED;
        }
    };
    let edit = settings::uninstall(&current);
    if dry_run {
        print!("{}", edit.rendered);
        return OK;
    }
    if edit.outcome == Outcome::NotPresent {
        println!("No andon guard hook found in {}.", path.display());
        return OK;
    }
    if let Err(e) = settings::write(&path, &edit.rendered) {
        eprintln!("andon: could not write {}: {e}", path.display());
        return FAILED;
    }
    println!("Removed the guard hook from {}.", path.display());
    OK
}

/// An absolute path, not the bare name, so the hook keeps working when Claude
/// Code is launched with a PATH that doesn't include the install directory.
fn guard_command() -> Option<String> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| eprintln!("andon: could not find my own path: {e}"))
        .ok()?;
    let exe = exe.display().to_string();
    Some(match exe.contains(char::is_whitespace) {
        true => format!("\"{exe}\" guard"),
        false => format!("{exe} guard"),
    })
}

fn not_found(prefix: &str, e: Resolve) -> ExitCode {
    eprintln!("andon: {prefix:?}: {e}.");
    if matches!(e, Resolve::NotFound) {
        eprintln!("Run `andon list` to see the open cords.");
    }
    FAILED
}

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
