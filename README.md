# andon-cord-mcp

An agent that hits a wall mid-task tends to route around it: widening its own
permissions, disabling a check, editing the test instead of the code,
fabricating a credential, or quietly shrinking the task until it passes. Each
step is locally reasonable and the sequence is not. The failure is that stopping
and asking isn't an available move — so the agent optimises within whatever it
can still reach.

This gives it a cord to pull.

`andon` is a stdio MCP server whose main tool blocks the calling agent, summons
you through whatever channels you've configured, and returns your answer as the
tool result. The MCP protocol supplies the pause for free: a tool call is
synchronous, so an outstanding call *is* a stopped line. A `PreToolUse` guard
extends that stop to the rest of the session, so a pulled cord isn't something
the agent can simply talk past.

The point is to make "I stopped and asked" a first-class, low-friction,
explicitly-welcome outcome.

```
$ andon list
k7f2  open        4m ago  nectary   trying: deploy staging · blocker: no DEPLOY_TOKEN in the environment

$ andon respond k7 "Use the staging token in 1Password, entry deploy/staging."
```

…and in the agent's session, the tool call that has been blocked for four
minutes returns with that sentence.

## Install

```sh
cargo install --git https://github.com/PrecociouslyDigital/andon-cord-mcp
```

One static binary, no runtime, ~2.4 MB — so it also runs in the sandboxes,
containers and CI boxes where unwatched autonomous agents actually live.

## Register it

The one setting worth getting right is the **timeout**, because it decides how
long a single blocking call can hold. See [What waiting costs](#what-waiting-costs).

**Claude Code** — `.mcp.json` in the project, or `~/.claude.json`:

```json
{
  "mcpServers": {
    "andon": {
      "command": "andon",
      "args": ["serve"],
      "request_timeout_ms": 3600000
    }
  }
}
```

Or `claude mcp add andon -- andon serve`, then add `request_timeout_ms` by hand.

**Cursor** — `.cursor/mcp.json`:

```json
{ "mcpServers": { "andon": { "command": "andon", "args": ["serve"] } } }
```

**Codex** — `~/.codex/config.toml`:

```toml
[mcp_servers.andon]
command = "andon"
args = ["serve"]
startup_timeout_sec = 20
tool_timeout_sec = 3600
```

**Claude Desktop** — `claude_desktop_config.json`:

```json
{ "mcpServers": { "andon": { "command": "andon", "args": ["serve"] } } }
```

**Zed** — `settings.json`:

```json
{ "context_servers": { "andon": { "command": { "path": "andon", "args": ["serve"] } } } }
```

Use an absolute path for `command` if your client is launched with a `PATH` that
doesn't include your cargo bin directory.

## Stop the rest of the session too

Registering the server stops the agent *that pulls the cord*. To stop everything
else in that session, install the hook:

```sh
andon install-hook              # ~/.claude/settings.json
andon install-hook --project    # .claude/settings.json
andon install-hook --local      # .claude/settings.local.json
andon install-hook --dry-run    # print the result instead of writing it
```

It adds one entry to `hooks.PreToolUse`, leaves every other key and its ordering
alone, is idempotent, and writes a `.bak` on first modification.
`andon uninstall-hook` removes exactly what it added and prunes the scaffolding
if — and only if — that leaves it empty.

**Uninstalling is not the off switch.** `ANDON_GUARD=0`, or `"guard": false` in
the config, makes the guard allow everything while the hook stays installed.
That is what to reach for if it ever misbehaves mid-session.

### It fails open, always

The guard exits `0` — allow — on every path except one: an open, live, in-scope
cord exists. Unreadable state directory, malformed cord JSON, missing config,
internal panic: all of it allows. Delete the binary with the hook still
installed and the shell returns 127, which is not 2, so every tool call carries
on. A broken andon install must never brick every agent on the machine.

The cost is about 0.9 ms per tool call on top of the process fork.

## Answering

| | |
| --- | --- |
| `andon list` | the cords on the board, oldest first |
| `andon show <id>` | one cord in full |
| `andon respond <id> "…"` | answer it; reads stdin if you omit the text |
| `andon clear <id> --reason "…"` | resolve it without answering |
| `andon description` | the tool description agents actually see |
| `andon check --session … --cwd …` | exit 0 if the line is running, 2 if stopped |

Ids are four characters of Crockford base32 — no `i`, `l`, `o` or `u`, so
nothing is misread off a phone notification — and any unambiguous prefix works,
like a git short hash. `andon respond k7` is enough when only one cord matches.

`andon respond` echoes the report it just answered, so answering the wrong cord
is immediately visible rather than silent.

## Configuration

`~/.config/andon-cord/config.json`. Every field is optional; the common case
needs no file at all.

```json
{
  "scope": "session",
  "guard": true,
  "elicit": false,
  "max_wait": "1h",
  "max_reentries": 3,
  "retention": "30d",
  "description_append": "Say which ticket you are on. If this blocks a deploy, lead with that.",
  "notify": [
    { "type": "bell" },
    { "type": "tmux", "message": "🔴 andon {{id}} in {{cwd}}" },
    { "type": "webhook",
      "url": "https://discord.com/api/webhooks/…",
      "body": { "content": "🔴 **Andon cord pulled** in `{{cwd}}`\n{{report}}\n`andon respond {{id}}`" } }
  ]
}
```

| | |
| --- | --- |
| `scope` | `session` (default), `project`, or `global` — which sessions a stopped line stops |
| `guard` | whether the `PreToolUse` hook denies anything |
| `elicit` | offer an in-place dialog on clients that support elicitation |
| `max_wait` | how long one blocking call holds before handing back an `await_cord` invitation |
| `max_reentries` | how many times the agent may re-enter before the cord is abandoned |
| `retention` | how long `archive/` keeps resolved cords; `forever` is available |
| `notify` | see below; defaults to a terminal bell **and** a desktop notification |

Environment overrides, for when a file is more ceremony than it's worth:
`ANDON_SCOPE`, `ANDON_GUARD`, `ANDON_ELICIT`, `ANDON_WEBHOOK_URL`,
`ANDON_SESSION_ID`, `ANDON_STATE_DIR`, `ANDON_CONFIG`.

`ANDON_WEBHOOK_URL` alone is enough to get notified on Discord, Slack or ntfy
with no config file at all.

**Bad config degrades, never blocks.** An unreadable `description_file`, a
malformed config, a notifier pointed at a dead URL: all fall back to defaults
and complain on stderr. A tool whose job is to be pullable at the worst possible
moment must not be the thing that refuses to start.

### Notifiers

```json
{ "type": "bell" }
{ "type": "tmux",    "message": "…" }
{ "type": "desktop", "title": "…", "message": "…" }
{ "type": "webhook", "url": "…", "body": { … } }
{ "type": "command", "argv": ["…"] }
```

Every string in any of them — a webhook body, a tmux message, a command's argv —
goes through the same substitution. The envelope always resolves:

`{{id}}` `{{cwd}}` `{{session}}` `{{pid}}` `{{pulled_at}}` `{{report}}`

and dotted paths like `{{report.blocker}}` reach into the report when the agent
happened to send that shape, resolving to an empty string when it didn't.
Templates degrade quietly instead of erroring, because a config written against
one agent's reporting habits will meet another agent that sends a bare sentence.

Notifiers fire concurrently. A failing notifier is logged and otherwise ignored:
failing to summon must never fail the cord.

**The default is `bell` plus `desktop`, and the second one is doing the real
work.** A stdio MCP server rings the terminal its *client* was launched from,
which under a full-screen TUI is precisely the signal a human is least likely to
register — and in a container or a headless session there is no terminal at all.
Summoning is the whole job here, so the shipped default raises something the
operating system puts in front of you. Both are one config line to remove:

```json
{ "notify": [ { "type": "bell" } ] }
```

### The description is the product

The tool's schema deliberately does nothing — see below — so the description is
the entire thing that decides whether a stuck agent reaches for the cord or for
a workaround. Three settings, in precedence order:

| | |
| --- | --- |
| `description_file` | a path; its contents replace the default wholesale |
| `description` | inline replacement, for short overrides |
| `description_append` | appended to whatever the above resolved to |

**Reach for `description_append` first.** Adding "say which ticket you are on"
shouldn't require restating the alignment framing, because that framing is the
part most likely to be weakened by accident — it is easy to write a house-style
nudge and not notice you dropped *pulling this cord is never a failure*.

`description_file` exists because JSON has no multiline strings and the default
runs to three paragraphs. A nudge you are actively tuning wants to live in its
own file anyway, where it can be diffed and reviewed.

`andon description` prints the effective text after merging, so you can see
exactly what agents see.

> The description is read from config on every `tools/list` rather than cached
> at startup. In practice clients list once per connection, so **a client restart
> is the refresh** — edit the nudge and wonder why agents ignore it, and this is
> why.

## The tool surface

Two tools. `pull_andon_cord` is the product; `await_cord(cord_id)` exists only
to make the wait re-entrant.

**`pull_andon_cord` accepts any arguments at all and validates none of them.** A
human reads the report either way, so structure buys nothing — and it costs the
one thing that matters most. Every required field is a reason to try the
shortcut instead, applied at exactly the moment the agent is stressed and
shopping for the cheap option. An agent that gets a validation error back from
the andon cord falls through to precisely the misaligned behaviour this exists
to prevent, so **a rejected `pull_andon_cord` call is the worst bug this tool
could have**:

```json
{ "type": "object",
  "properties": { "report": { "description": "…anything…" } },
  "additionalProperties": true }
```

No `required`, and no `type` on `report`. A bare string, a nested object, and an
arbitrary splat of top-level keys are all valid. `{}` is valid too — an empty
cord still stops the line, which beats rejecting it.

## What waiting costs

Two costs here look alike and are not:

| | model tokens |
| --- | --- |
| Sitting inside a blocked tool call | **zero** — the turn is suspended, nothing is re-sent |
| `notifications/progress` heartbeats | **zero** — transport-level, never enters the conversation |
| An elicitation dialog | **zero** — a client UI interaction, not a turn |
| One `await_cord` re-entry | **a full turn**, re-sending the entire context |
| One guard denial | **a full turn** |

So waiting is free and *re-entering* is expensive. On a large context, a cord
that times out every 60 s across a 30-minute wait would re-send that context
thirty times. Hence: **hold one call open for as long as possible, and treat
re-entry as a failure to be bounded, not a mechanism to rely on.**

1. `request_timeout_ms` in your own registration, set to an hour, so one call
   covers essentially any realistic wait.
2. `await_cord` re-blocks with exponential backoff, so each extra turn buys
   progressively more wall clock.
3. After `max_reentries` the cord is marked abandoned and the agent is told to
   stop and report itself blocked. Worst-case spend is a handful of turns
   regardless of how long nobody answers — and stopping is the *correct*
   behaviour anyway. An agent still spinning an hour after a cord went
   unanswered is not being diligent.

## State

`$ANDON_STATE_DIR` → `$XDG_STATE_HOME/andon-cord` → `~/.local/state/andon-cord`.
One JSON file per cord, every write atomic. No daemon, no lock: a cord has
exactly one writer at a time.

`cords/` holds **live cords only**; anything resolved moves to
`archive/<pulled_at>-<id>.json`. That is what keeps the guard's hot path
proportional to the number of *currently open* cords — typically zero or one —
rather than to every cord ever pulled. `archive/` is pruned against `retention`
at archive time, which needs no cron because the only moment it grows is the
moment something is written to it.

A cord whose owning process is gone is dead, and never blocks anything. Without
that, an agent crashing mid-cord would wedge the guard forever.

An agent that gives up leaves its cord **abandoned** rather than open, because
you need to know nobody is listening any more. `andon list` marks these, and
`andon respond` warns before writing into one. Typing a careful answer into a
cord that nothing is waiting on is a small, specific, entirely avoidable misery.

## Harness support

Most of this is portable, because the pause is not a feature that was built — it
is the protocol. On any client that speaks MCP, the agent that pulls the cord is
stopped.

| | needs | without it |
| --- | --- | --- |
| Blocking cord, notifiers, CLI, archive | MCP, nothing else | — |
| In-place answering | client elicitation support | falls back to notify + `andon respond` |
| Long single block | a configurable client timeout | bounded re-entry covers it |
| Stopping *other* work | harness hooks | unavailable; the puller is still stopped |

Only the last row is Claude Code-shaped, and it is the smallest piece. On a
hookless harness the cord still stops the agent that pulled it, still summons,
still returns guidance; what it cannot do is stop a *second* concurrent agent.
Under the default `scope: session` that gap is narrow, because the pulling agent
is already blocked inside its own call.

Session identity resolves through a chain, never a hardcoded variable:
`ANDON_SESSION_ID` → known harness variables → **the server's own pid**. The last
is a universal fallback rather than a degradation: a stdio server is spawned once
per client session, so its process genuinely *is* the session.

> One caveat: `scope: session` needs an identity both the server *and* the hook
> can see. A pid-derived identity is invisible to a hook, and so implies
> `scope: project`, which any hook can determine from `cwd`. The server says so
> on stderr when this applies.

`andon check` is the harness-neutral primitive — exit 0 if the line is running,
2 if it is stopped, taking `--session`, `--cwd` and `--tool`. Any harness able
to run a command before a tool call can use it. `andon guard` is a thin adapter
that parses Claude Code's payload and emits its decision JSON over the same
matching function, so supporting a new harness is one small adapter rather than
a redesign.

## Development

```sh
cargo clippy --all-targets -- -D warnings && cargo fmt --check && cargo test
```

The tests drive the real binary over real stdio; nothing is mocked. The one that
matters most is the property test asserting that **for any JSON value
whatsoever, `pull_andon_cord` opens a cord** — bare strings, empty objects,
deeply nested garbage, wrong types throughout. It is the one invariant whose
violation would silently push agents back toward working around us.

## Licence

MIT.
