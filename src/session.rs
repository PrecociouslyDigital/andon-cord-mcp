//! Who is asking.
//!
//! Session identity resolves through a chain rather than a hardcoded variable,
//! so a harness we have never heard of can opt in without us shipping code for
//! it.

/// `ANDON_SESSION_ID` → a known harness variable → the server's own pid.
///
/// The last is a universal fallback rather than a degradation: a stdio server
/// is spawned once per client session, so its process genuinely *is* the
/// session.
///
/// The caveat, worth knowing before setting `scope: session`: that scope needs
/// an identity both the server *and* the hook can see. A pid-derived identity
/// is invisible to a hook, and so implies `scope: project`, which any hook can
/// determine from `cwd`.
pub fn id() -> String {
    const HARNESS_VARS: &[&str] = &["CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID"];

    if let Some(id) = var("ANDON_SESSION_ID") {
        return id;
    }
    HARNESS_VARS
        .iter()
        .find_map(|key| var(key))
        .unwrap_or_else(|| format!("pid:{}", std::process::id()))
}

/// True when [`id`] fell all the way through to the pid.
pub fn is_pid_derived(id: &str) -> bool {
    id.starts_with("pid:")
}

pub fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
