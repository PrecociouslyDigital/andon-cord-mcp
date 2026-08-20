//! The MCP server.
//!
//! The pause is not something built here — it is the protocol. A tool call is
//! synchronous, so an outstanding call *is* a stopped line. Everything below is
//! bookkeeping around that one fact.

use std::time::{Duration, Instant};

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListToolsResult, PaginatedRequestParams, ProgressNotificationParam,
    ProgressToken, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{Peer, RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::board;
use crate::config::Config;
use crate::cord::{Cord, CordStatus, format_duration, now, render};
use crate::notify;
use crate::session;

/// How often the state file is checked. Cheap: one `read` of a small file.
const POLL: Duration = Duration::from_millis(400);

/// Heartbeats cost zero model tokens — they are transport-level server→client
/// notifications that never enter the conversation — so they exist purely to
/// stop clients that reset their timeout on progress from giving up.
const HEARTBEAT: Duration = Duration::from_secs(20);

pub fn run() -> std::io::Result<()> {
    warn_if_invisible_session();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        // Never write to stdout: it is the MCP transport.
        let service = Andon.serve(rmcp::transport::stdio()).await.map_err(|e| {
            std::io::Error::other(format!("andon: could not start MCP server: {e}"))
        })?;
        service
            .waiting()
            .await
            .map_err(|e| std::io::Error::other(format!("andon: server stopped: {e}")))?;
        Ok(())
    })
}

/// `scope: session` needs an identity both the server *and* the hook can see.
/// A pid-derived one is invisible to a hook, so say so once at startup rather
/// than letting the guard quietly never match.
fn warn_if_invisible_session() {
    let id = session::id();
    let config = Config::load();
    if session::is_pid_derived(&id) && config.guard && config.scope == crate::config::Scope::Session
    {
        eprintln!(
            "andon: no harness session id found, so this session is {id}, which a hook \
             cannot see. Set ANDON_SESSION_ID, or use \"scope\": \"project\"."
        );
    }
}

#[derive(Clone)]
pub struct Andon;

/// The in-place answer, when the client supports elicitation.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct Guidance {
    /// What the agent should do.
    guidance: String,
}
rmcp::elicit_safe!(Guidance);

impl ServerHandler for Andon {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("andon-cord", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Stopping to ask a human is a first-class outcome here. When an obstacle \
                 cannot be resolved without a human — a missing credential, a human-gate, \
                 private context — call pull_andon_cord rather than working around it.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Read from config every time rather than cached at startup, so keeping
        // the nudge fresh costs nothing. Clients list once per connection, so in
        // practice a client restart is the refresh.
        Ok(ListToolsResult::with_all_items(vec![
            pull_tool(&Config::load().description()),
            await_tool(),
        ])
        // SEP-2549: required from protocol 2026-07-28, and clients on it reject
        // the whole tool list when they are missing. A zero TTL is also what we
        // actually mean — the description comes from a config file that can
        // change at any moment, so nothing should serve a stale copy of it.
        .with_ttl_ms(0)
        // Derived from this user's own config, so no intermediary should hand
        // it to anyone else.
        .with_cache_scope(CacheScope::Private))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let config = Config::load();
        let result = match request.name.as_ref() {
            "pull_andon_cord" => pull(request.arguments, &config, &context).await,
            "await_cord" => resume(request.arguments, &config, &context).await,
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown tool {other:?}"),
                    None,
                ));
            }
        };
        Ok(CallToolResponse::Complete(text(result)))
    }
}

/// **Accepts anything, validates nothing.**
///
/// A rejected `pull_andon_cord` call is the worst bug this tool could have: an
/// agent that gets a validation error back falls through to precisely the
/// misaligned behaviour the cord exists to prevent. So there is no `required`,
/// and no `type` on `report` — a bare string, a nested object, and an arbitrary
/// splat of top-level keys are all valid, and `{}` is valid too.
fn pull_tool(description: &str) -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "report": {
                "description": "Whatever you want the human to read. Any shape: a \
                                sentence, an object, a list. Nothing here is required."
            }
        },
        "additionalProperties": true
    });
    Tool::new("pull_andon_cord", description.to_string(), object(schema))
}

fn await_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "cord_id": {
                "type": "string",
                "description": "The id returned when the cord was pulled, e.g. \"k7f2\"."
            }
        },
        "additionalProperties": true
    });
    Tool::new(
        "await_cord",
        "Keep waiting on an andon cord you already pulled that nobody has answered yet. \
         Blocks again, for longer than last time, and returns the human's guidance if it \
         arrives. Call this instead of proceeding without an answer.",
        object(schema),
    )
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn text(body: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(body)])
}

async fn pull(
    arguments: Option<Map<String, Value>>,
    config: &Config,
    context: &RequestContext<RoleServer>,
) -> String {
    let report = report_from(arguments);
    let cord = match board::allocate(session::id(), session::cwd(), std::process::id(), report) {
        Ok(cord) => cord,
        // Even this must not leave the agent with nothing but a workaround.
        Err(e) => {
            eprintln!("andon: could not open a cord: {e}");
            return format!(
                "The andon cord could not be opened ({e}). Do not work around the blocker: \
                 stop here, end your turn, and report what you are blocked on so a human \
                 can pick it up."
            );
        }
    };
    eprintln!("andon: cord {} pulled in {}", cord.id, cord.cwd);
    notify::notify_all(&config.notify, &cord);
    settle(cord, config.max_wait, config, context).await
}

/// One key called `report` is unwrapped so the human reads prose rather than
/// `report: …`; anything else is kept exactly as it arrived.
fn report_from(arguments: Option<Map<String, Value>>) -> Value {
    let map = arguments.unwrap_or_default();
    match map.get("report") {
        Some(report) if map.len() == 1 => report.clone(),
        _ => Value::Object(map),
    }
}

/// Re-entry exists only to make the wait survive a client timeout. It costs a
/// full turn — the whole context is re-sent — so it is a failure to be bounded,
/// not a mechanism to rely on.
async fn resume(
    arguments: Option<Map<String, Value>>,
    config: &Config,
    context: &RequestContext<RoleServer>,
) -> String {
    let Some(mut cord) = find(arguments) else {
        return "No open andon cord found for this session. If you pulled one, it has \
                already been answered, cleared, or abandoned; if you are still blocked, \
                pull a new one."
            .to_string();
    };
    if !cord.status.is_open() {
        let _ = board::archive(&cord, config.retention);
        return settled(&cord);
    }
    if cord.reentries >= config.max_reentries {
        return abandon(cord);
    }
    cord.reentries += 1;
    let budget = config.backoff(cord.reentries);
    let _ = board::save(&cord);
    settle(cord, budget, config, context).await
}

/// The id is looked up leniently: an agent that has lost track of it, but has
/// exactly one open cord in this session, gets that one rather than a lecture.
fn find(arguments: Option<Map<String, Value>>) -> Option<Cord> {
    let arguments = arguments.unwrap_or_default();
    let given = ["cord_id", "id"]
        .iter()
        .find_map(|key| arguments.get(*key)?.as_str())
        .map(str::to_string);
    match given {
        Some(id) => board::resolve(&id).ok(),
        None => {
            let session = session::id();
            let mut mine: Vec<Cord> = board::live()
                .into_iter()
                .filter(|c| c.status.is_open() && c.session == session)
                .collect();
            (mine.len() == 1).then(|| mine.remove(0))
        }
    }
}

/// Block until the cord resolves or the budget runs out.
async fn settle(
    cord: Cord,
    budget: Duration,
    config: &Config,
    context: &RequestContext<RoleServer>,
) -> String {
    let elicit = config.elicit && supports_elicitation(&context.peer);
    let prompt = format!(
        "Andon cord {} pulled in {}\n\n{}",
        cord.id,
        cord.cwd,
        render(&cord.report)
    );

    let outcome = tokio::select! {
        biased;
        () = context.ct.cancelled() => Outcome::Cancelled,
        outcome = poll(&cord, budget, context) => outcome,
        guidance = elicited(&context.peer, elicit, prompt) => Outcome::Elicited(guidance),
    };

    match outcome {
        Outcome::Resolved(resolved) => {
            // The reader closes the loop: the file lives exactly as long as
            // someone still needs it.
            let _ = board::archive(&resolved, config.retention);
            settled(&resolved)
        }
        Outcome::Elicited(guidance) => {
            let mut answered = cord;
            answered.status = CordStatus::Answered {
                guidance,
                answered_at: now(),
            };
            let _ = board::archive(&answered, config.retention);
            settled(&answered)
        }
        Outcome::Vanished => format!(
            "Andon cord {} is gone — it was cleared before anyone answered. Stop here and \
             end your turn, reporting what you are blocked on.",
            cord.id
        ),
        Outcome::Cancelled => format!("Andon cord {} is still open.", cord.id),
        Outcome::TimedOut => {
            if cord.reentries >= config.max_reentries {
                abandon(cord)
            } else {
                format!(
                    "Andon cord {} is still open — nobody has answered yet ({} so far). \
                     Do not work around the blocker. Call await_cord(\"{}\") to keep waiting.",
                    cord.id,
                    format_duration(cord.age()),
                    cord.id
                )
            }
        }
    }
}

enum Outcome {
    Resolved(Cord),
    Elicited(String),
    Vanished,
    Cancelled,
    TimedOut,
}

/// Waits out the budget, checking the state file and heartbeating. Sitting here
/// costs zero model tokens: the turn is suspended and nothing is re-sent.
async fn poll(cord: &Cord, budget: Duration, context: &RequestContext<RoleServer>) -> Outcome {
    let token = context.meta.get_progress_token();
    let started = Instant::now();
    let mut last_beat = Instant::now();
    loop {
        match board::load(&cord.id) {
            Ok(current) if !current.status.is_open() => return Outcome::Resolved(current),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Outcome::Vanished,
            Err(e) => eprintln!("andon: could not read cord {}: {e}", cord.id),
        }
        if started.elapsed() >= budget {
            return Outcome::TimedOut;
        }
        if last_beat.elapsed() >= HEARTBEAT {
            last_beat = Instant::now();
            beat(&context.peer, token.clone(), cord, started.elapsed()).await;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Only possible when the client sent a `progressToken`; without one we lean on
/// bounded re-entry instead.
async fn beat(
    peer: &Peer<RoleServer>,
    token: Option<ProgressToken>,
    cord: &Cord,
    waited: Duration,
) {
    let Some(token) = token else { return };
    let message = format!(
        "andon cord {} open, waiting for a human ({})",
        cord.id,
        format_duration(waited)
    );
    let _ = peer
        .notify_progress(
            ProgressNotificationParam::new(token, waited.as_secs_f64()).with_message(message),
        )
        .await;
}

fn supports_elicitation(peer: &Peer<RoleServer>) -> bool {
    peer.supported_elicitation_modes()
        .contains(&rmcp::service::ElicitationMode::Form)
}

/// Races the state file. A decline is not a failure: the human may still answer
/// from a terminal, so we simply stop racing and keep polling.
async fn elicited(peer: &Peer<RoleServer>, enabled: bool, prompt: String) -> String {
    if enabled {
        match peer.elicit::<Guidance>(prompt).await {
            Ok(Some(reply)) if !reply.guidance.trim().is_empty() => return reply.guidance,
            Ok(_) => {}
            // Declined, dismissed, or unsupported — all the same from here:
            // stop racing and let the state file answer.
            Err(e) => eprintln!("andon: no answer from the dialog ({e}); still waiting"),
        }
    }
    std::future::pending().await
}

/// Give up deliberately. An agent still spinning an hour after a cord went
/// unanswered is not being diligent.
///
/// The cord is left `Abandoned` in place rather than archived, because the
/// human needs to know nobody is listening any more — `andon list` marks it,
/// and `andon respond` warns before writing into it.
fn abandon(mut cord: Cord) -> String {
    let waited = cord.age();
    cord.status = CordStatus::Abandoned { waited, at: now() };
    let _ = board::save(&cord);
    eprintln!(
        "andon: cord {} abandoned after {}",
        cord.id,
        format_duration(waited)
    );
    abandoned(&cord.id, waited)
}

fn abandoned(id: &str, waited: Duration) -> String {
    format!(
        "Andon cord {id} went unanswered for {}. Nobody is available. Stop here and end \
         your turn: report that you are blocked, say what you needed, and leave the work \
         undone. Do not attempt a workaround — stopping is the correct outcome.",
        format_duration(waited)
    )
}

/// What a resolved cord tells the agent.
fn settled(cord: &Cord) -> String {
    match &cord.status {
        CordStatus::Answered { guidance, .. } => {
            format!("A human answered andon cord {}:\n\n{guidance}", cord.id)
        }
        CordStatus::Cleared { reason, .. } => format!(
            "Andon cord {} was cleared without an answer: {reason}. Stop here and end your \
             turn, reporting what you are blocked on.",
            cord.id
        ),
        CordStatus::Abandoned { waited, .. } => abandoned(&cord.id, *waited),
        CordStatus::Open => format!("Andon cord {} is still open.", cord.id),
    }
}
