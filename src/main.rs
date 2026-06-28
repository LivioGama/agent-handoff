mod adapters;
mod format;
mod model;
mod util;

use adapters::{registry, Query};
use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use model::{Agent, SessionRef};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Extract conversation transcripts from local AI coding agent logs — no LLM, pure file parsing.
#[derive(Parser)]
#[command(name = "agent-handoff", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List sessions for the current directory across all agents (newest first).
    List {
        /// Restrict to one agent: claude | codex | opencode | cursor | devin
        #[arg(long)]
        agent: Option<String>,
        /// Working directory to scope sessions to (default: current dir).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Keep only the N most-recently-modified sessions (your open terminals).
        #[arg(long)]
        active: Option<usize>,
        /// Recency window, e.g. 5m, 2h, 3d (default: 1d). Skips opening older files.
        #[arg(long)]
        since: Option<String>,
        /// Ignore the recency window and scan all history.
        #[arg(long)]
        all: bool,
        /// Emit JSON instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Export a session's transcript: saves a Markdown file to ~/.agent-handoff/ and copies it to the clipboard.
    Export {
        /// Session id to export. Omit for the newest matching session.
        id: Option<String>,
        /// Restrict to one agent: claude | codex | opencode | cursor | devin
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Include tool calls and results (default: prompts + answers only).
        #[arg(long)]
        tools: bool,
        /// Recency window, e.g. 5m, 2h, 3d (default: 1d). Ignored when an id is given.
        #[arg(long)]
        since: Option<String>,
        /// Ignore the recency window and scan all history.
        #[arg(long)]
        all: bool,
        /// Print Markdown to stdout instead of saving a file / copying to clipboard (for piping).
        #[arg(long)]
        stdout: bool,
        /// Emit the normalized model as JSON to stdout instead of Markdown.
        #[arg(long)]
        json: bool,
    },
}

/// Default recency window applied when neither `--since` nor `--all` is given.
const DEFAULT_SINCE: &str = "1d";

/// Resolve the effective recency cutoff: `--all` => none; else `--since` or the default window.
fn since_cutoff(since: &Option<String>, all: bool) -> Result<Option<SystemTime>> {
    if all {
        return Ok(None);
    }
    let spec = since.clone().unwrap_or_else(|| DEFAULT_SINCE.to_string());
    let d: Duration = util::parse_duration(&spec)
        .ok_or_else(|| anyhow!("invalid --since '{}' (use e.g. 30s, 5m, 2h, 3d)", spec))?;
    Ok(Some(SystemTime::now() - d))
}

fn resolve_agent(s: &Option<String>) -> Result<Option<Agent>> {
    match s {
        None => Ok(None),
        Some(v) => Agent::parse(v).map(Some).ok_or_else(|| {
            anyhow!(
                "unknown agent '{}' (use claude|codex|opencode|cursor|devin)",
                v
            )
        }),
    }
}

fn cwd_of(opt: &Option<PathBuf>) -> Result<PathBuf> {
    Ok(match opt {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    })
}

/// Collect + sort (newest first) sessions across the selected adapters for a query.
fn collect(agent: Option<Agent>, q: &Query) -> Vec<SessionRef> {
    let mut all = Vec::new();
    for adapter in registry(agent) {
        if let Ok(sessions) = adapter.discover(q) {
            all.extend(sessions);
        }
    }
    all.sort_by(|a, b| b.modified.cmp(&a.modified));
    all
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::List {
            agent,
            cwd,
            active,
            since,
            all,
            json,
        } => {
            let agent = resolve_agent(&agent)?;
            let cwd = cwd_of(&cwd)?;
            let q = Query {
                cwd: &cwd,
                since: since_cutoff(&since, all)?,
            };
            let mut sessions = collect(agent, &q);
            if let Some(n) = active {
                sessions.truncate(n);
            }
            if sessions.is_empty() {
                eprintln!(
                    "no sessions in the last {} for {} (try --since 7d or --all)",
                    since.as_deref().unwrap_or(DEFAULT_SINCE),
                    cwd.display()
                );
                return Ok(());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                print!("{}", format::roster(&sessions));
            }
        }
        Cmd::Export {
            id,
            agent,
            cwd,
            tools,
            since,
            all,
            stdout,
            json,
        } => {
            let agent = resolve_agent(&agent)?;
            let cwd = cwd_of(&cwd)?;
            // An explicit id should be findable regardless of recency.
            let cutoff = if id.is_some() {
                None
            } else {
                since_cutoff(&since, all)?
            };
            let q = Query {
                cwd: &cwd,
                since: cutoff,
            };
            let sessions = collect(agent, &q);
            let chosen = match &id {
                Some(want) => sessions.iter().find(|s| s.id == *want).ok_or_else(|| {
                    anyhow!("session id '{}' not found for {}", want, cwd.display())
                })?,
                None => sessions.first().ok_or_else(|| {
                    anyhow!(
                        "no sessions in the last {} for {} (try --since 7d or --all)",
                        since.as_deref().unwrap_or(DEFAULT_SINCE),
                        cwd.display()
                    )
                })?,
            };
            let adapter = registry(Some(chosen.agent))
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no adapter for {:?}", chosen.agent))?;
            let turns = adapter.read(chosen, tools)?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "agent": chosen.agent,
                        "id": chosen.id,
                        "cwd": chosen.cwd,
                        "branch": chosen.branch,
                        "turns": turns,
                    }))?
                );
                return Ok(());
            }

            let md = format::markdown(chosen, &turns);
            if stdout {
                print!("{}", md);
                return Ok(());
            }

            // Default: save a Markdown file to ~/.agent-handoff/ AND copy it to the clipboard.
            // Timestamp-first name sorts chronologically: 2026-06-28_16-34-39-claude-2fbe7e2a.md
            let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
            let short: String = chosen.id.chars().take(8).collect();
            let fname = format!("{}-{}-{}.md", stamp, chosen.agent.label(), short);
            let path = util::store_dir().join(&fname);
            std::fs::write(&path, &md)
                .map_err(|e| anyhow!("failed to write {}: {}", path.display(), e))?;
            let clip = match util::copy_to_clipboard(&md) {
                Ok(()) => "copied to clipboard",
                Err(_) => "clipboard unavailable",
            };
            let _ = util::open_path(&path); // open in the default markdown viewer
            eprintln!(
                "Saved {} ({} turns, {} bytes), {}, and opened it.",
                path.display(),
                turns.len(),
                md.len(),
                clip
            );
        }
    }
    Ok(())
}
