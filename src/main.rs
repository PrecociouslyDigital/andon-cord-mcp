//! `andon` — one binary, a few subcommands.
//!
//! Dispatch happens before any runtime is constructed, so `andon guard` never
//! builds a tokio runtime and never touches `rmcp`. That separation is the
//! reason the guard can afford to run on every single tool call.

mod board;
mod cli;
mod config;
mod cord;
mod guard;
mod notify;
mod server;
mod session;
mod settings;
#[cfg(test)]
mod testing;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use settings::Target;

#[derive(Parser)]
#[command(
    name = "andon",
    version,
    about = "An andon cord for coding agents: stop the line and ask a human."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server on stdio. This is what goes in your client config.
    Serve,
    /// Show the cords currently on the board.
    List,
    /// Show one cord in full.
    Show { cord: String },
    /// Answer a cord. Reads the guidance from stdin if you don't pass it.
    Respond {
        cord: String,
        /// The guidance to hand back to the agent.
        guidance: Vec<String>,
    },
    /// Resolve a cord without answering it.
    Clear {
        cord: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Exit 0 if the line is running, 2 if a cord is stopping it.
    Check {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// The tool about to be called, if the harness knows it.
        #[arg(long)]
        tool: Option<String>,
    },
    /// Print the tool description agents actually see.
    Description,
    /// Claude Code `PreToolUse` adapter. Reads the hook payload on stdin.
    Guard,
    /// Add the guard hook to a Claude Code settings file.
    InstallHook(HookArgs),
    /// Remove the guard hook again, leaving everything else untouched.
    UninstallHook(HookArgs),
}

#[derive(Args)]
struct HookArgs {
    /// Edit .claude/settings.json instead of ~/.claude/settings.json.
    #[arg(long, group = "scope")]
    project: bool,
    /// Edit .claude/settings.local.json instead.
    #[arg(long, group = "scope")]
    local: bool,
    /// Print the resulting file instead of writing it.
    #[arg(long)]
    dry_run: bool,
}

impl HookArgs {
    fn target(&self) -> Target {
        match (self.project, self.local) {
            (true, _) => Target::Project,
            (_, true) => Target::Local,
            _ => Target::User,
        }
    }
}

fn main() -> ExitCode {
    match Cli::parse().command {
        // The hot path: no runtime, no rmcp, no allocation beyond the payload.
        Command::Guard => guard::claude_code(),
        Command::Check { session, cwd, tool } => cli::check(session, cwd, tool),
        Command::List => cli::list(),
        Command::Show { cord } => cli::show(&cord),
        Command::Respond { cord, guidance } => {
            let guidance = (!guidance.is_empty()).then(|| guidance.join(" "));
            cli::respond(&cord, guidance)
        }
        Command::Clear { cord, reason } => cli::clear(&cord, reason),
        Command::Description => cli::description(),
        Command::InstallHook(args) => cli::install_hook(args.target(), args.dry_run),
        Command::UninstallHook(args) => cli::uninstall_hook(args.target(), args.dry_run),
        Command::Serve => match server::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
    }
}
