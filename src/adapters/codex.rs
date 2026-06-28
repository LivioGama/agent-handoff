use super::{home, Adapter, Query};
use crate::model::{Agent, Role, SessionRef, Turn};
use crate::util::{fingerprint_line, git_branch, looks_like_noise};
use anyhow::Result;
use chrono::{DateTime, Datelike, Local};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

pub struct CodexAdapter;

type Ymd = (i32, u32, u32);

fn ymd_of(t: SystemTime) -> Ymd {
    let dt: DateTime<Local> = t.into();
    (dt.year(), dt.month(), dt.day())
}

/// Pull the role + flattened text out of a `response_item` message payload.
fn message_turn(payload: &Value, include_tools: bool) -> Option<Turn> {
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let role = match payload.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None, // skip developer/system
    };
    let mut parts = Vec::new();
    if let Some(arr) = payload.get("content").and_then(Value::as_array) {
        for b in arr {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                "input_text" | "output_text" | "text" => {
                    if let Some(s) = b.get("text").and_then(Value::as_str) {
                        parts.push(s.to_string());
                    }
                }
                other if include_tools => {
                    parts.push(format!("`[{}]`", other));
                }
                _ => {}
            }
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(Turn { role, text })
}

/// Keep a `sessions/<Y>/<M>/<D>` directory only if its (partial) date is >= the cutoff date.
/// Pruning whole year/month/day subtrees means stale sessions are never even stat-ed.
fn dir_after_cutoff(dir: &Path, root: &Path, cutoff: Ymd) -> bool {
    let Ok(rel) = dir.strip_prefix(root) else {
        return true;
    };
    let parts: Vec<u32> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str().and_then(|s| s.parse().ok()))
        .collect();
    let (cy, cm, cd) = cutoff;
    match parts.as_slice() {
        [y] => *y as i32 >= cy,
        [y, m] => (*y as i32, *m) >= (cy, cm),
        [y, m, d] => (*y as i32, *m, *d) >= (cy, cm, cd),
        _ => true,
    }
}

/// Parse the `YYYY-MM-DD` date out of an archived `rollout-<ISO>-<ulid>.jsonl` filename.
fn archived_file_date(name: &str) -> Option<Ymd> {
    let rest = name.strip_prefix("rollout-")?;
    let date = rest.get(..10)?; // YYYY-MM-DD
    let mut it = date.split('-');
    let y = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    Some((y, m, d))
}

/// Visit every Codex session `.jsonl`, pruning by date when a `since` cutoff is set.
fn each_session_file<F: FnMut(PathBuf)>(since: Option<SystemTime>, mut f: F) {
    let cutoff = since.map(ymd_of);
    let base = home().join(".codex");

    // sessions/: date-partitioned — prune old Y/M/D subtrees via filter_entry.
    let sessions = base.join("sessions");
    let sroot = sessions.clone();
    let walker = WalkDir::new(&sessions).into_iter().filter_entry(move |e| {
        if !e.file_type().is_dir() {
            return true;
        }
        match cutoff {
            Some(c) => dir_after_cutoff(e.path(), &sroot, c),
            None => true,
        }
    });
    for entry in walker.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            f(p.to_path_buf());
        }
    }

    // archived_sessions/: flat — the date is in the filename, so gate without stat-ing.
    let archived = base.join("archived_sessions");
    for entry in WalkDir::new(&archived).into_iter().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(c) = cutoff {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if let Some(d) = archived_file_date(name) {
                if d < c {
                    continue;
                }
            }
        }
        f(p.to_path_buf());
    }
}

impl Adapter for CodexAdapter {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    fn discover(&self, q: &Query) -> Result<Vec<SessionRef>> {
        let target = q.cwd.to_string_lossy().to_string();
        let branch = git_branch(&target); // resolve once per cwd, not per session

        // Phase 1 (cheap): collect candidate paths, date-pruned then stat-gated by mtime —
        // no file is opened yet, and stale date subtrees are never walked.
        let mut candidates: Vec<(PathBuf, SystemTime)> = Vec::new();
        each_session_file(q.since, |path| {
            let modified = fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if q.recent_enough(modified) {
                candidates.push((path, modified));
            }
        });

        // Phase 2 (IO-bound): peek header + fingerprint, parallelized across the candidate set.
        // Opening thousands of session files serially dominates wall time; threads hide the IO.
        let out = parallel_map(candidates, &target, &branch);
        Ok(out)
    }

    fn read(&self, session: &SessionRef, include_tools: bool) -> Result<Vec<Turn>> {
        let file = fs::File::open(&session.path)?;
        let mut turns = Vec::new();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let o: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if o.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            if let Some(payload) = o.get("payload") {
                if let Some(turn) = message_turn(payload, include_tools) {
                    turns.push(turn);
                }
            }
        }
        Ok(turns)
    }
}

/// Process candidate files in parallel: peek the header (cwd gate) then fingerprint-scan matches.
/// Uses scoped OS threads (no extra deps) chunked by available parallelism.
fn parallel_map(
    candidates: Vec<(PathBuf, SystemTime)>,
    target: &str,
    branch: &Option<String>,
) -> Vec<SessionRef> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(candidates.len());
    let chunk = candidates.len().div_ceil(threads);

    std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .chunks(chunk)
            .map(|slice| {
                scope.spawn(move || {
                    let mut local = Vec::new();
                    for (path, modified) in slice {
                        let Some((meta_cwd, meta_id)) = peek_meta(path, target) else {
                            continue;
                        };
                        if meta_cwd != target {
                            continue;
                        }
                        let (_, _, n, first, last) = scan(path);
                        local.push(SessionRef {
                            agent: Agent::Codex,
                            id: meta_id.unwrap_or_else(|| {
                                path.file_stem()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .to_string()
                            }),
                            path: path.clone(),
                            branch: branch.clone(),
                            cwd: meta_cwd,
                            modified: *modified,
                            prompts: n,
                            first,
                            last,
                        });
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Read only the leading records to extract (cwd, id) from the `session_meta` header.
/// A raw substring pre-check on `target` avoids JSON-parsing the (large) header of the
/// thousands of sessions that belong to other directories.
fn peek_meta(path: &Path, target: &str) -> Option<(String, Option<String>)> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines().map_while(Result::ok).take(5) {
        if !line.contains("session_meta") {
            continue;
        }
        if !line.contains(target) {
            return None; // header present but for a different cwd — skip without parsing
        }
        let o: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if o.get("type").and_then(Value::as_str) == Some("session_meta") {
            let p = o.get("payload")?;
            let cwd = p.get("cwd").and_then(Value::as_str)?.to_string();
            let id = p.get("id").and_then(Value::as_str).map(str::to_string);
            return Some((cwd, id));
        }
    }
    None
}

/// (cwd, session_id, #user-prompts, first, last)
fn scan(
    path: &Path,
) -> (
    Option<String>,
    Option<String>,
    usize,
    Option<String>,
    Option<String>,
) {
    let mut cwd = None;
    let mut id = None;
    let mut n = 0usize;
    let mut first = None;
    let mut last = None;
    if let Ok(file) = fs::File::open(path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let o: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match o.get("type").and_then(Value::as_str) {
                Some("session_meta") => {
                    if let Some(p) = o.get("payload") {
                        if let Some(c) = p.get("cwd").and_then(Value::as_str) {
                            cwd = Some(c.to_string());
                        }
                        if let Some(i) = p.get("id").and_then(Value::as_str) {
                            id = Some(i.to_string());
                        }
                    }
                }
                Some("response_item") => {
                    let payload = o.get("payload");
                    let is_user =
                        payload.and_then(|p| p.get("role")).and_then(Value::as_str) == Some("user");
                    if is_user {
                        if let Some(arr) = payload
                            .and_then(|p| p.get("content"))
                            .and_then(Value::as_array)
                        {
                            let text: String = arr
                                .iter()
                                .filter_map(|b| b.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join(" ");
                            if !looks_like_noise(&text) {
                                n += 1;
                                let fp = fingerprint_line(&text, 70);
                                if first.is_none() {
                                    first = Some(fp.clone());
                                }
                                last = Some(fp);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    (cwd, id, n, first, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/sample.jsonl")
    }

    fn fixture() -> SessionRef {
        SessionRef {
            agent: Agent::Codex,
            id: "abc123".into(),
            path: fixture_path(),
            cwd: "/tmp/proj".into(),
            branch: None,
            modified: SystemTime::UNIX_EPOCH,
            prompts: 0,
            first: None,
            last: None,
        }
    }

    #[test]
    fn read_keeps_user_and_assistant_skips_developer() {
        let turns = CodexAdapter.read(&fixture(), false).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].text, "hello codex");
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(turns[1].text, "hi from codex");
    }

    #[test]
    fn peek_meta_gate_matches_and_rejects() {
        assert!(peek_meta(&fixture_path(), "/tmp/proj").is_some());
        assert!(peek_meta(&fixture_path(), "/other/dir").is_none());
    }

    #[test]
    fn archived_date_parsing() {
        assert_eq!(
            archived_file_date("rollout-2026-06-04T20-10-48-019e93d4.jsonl"),
            Some((2026, 6, 4))
        );
        assert_eq!(archived_file_date("not-a-rollout.jsonl"), None);
    }

    #[test]
    fn dir_pruning_by_date() {
        let root = Path::new("/sessions");
        // cutoff 2026-06-10
        let c = (2026, 6, 10);
        assert!(dir_after_cutoff(Path::new("/sessions/2026"), root, c)); // same year, keep
        assert!(!dir_after_cutoff(Path::new("/sessions/2025"), root, c)); // older year, prune
        assert!(!dir_after_cutoff(Path::new("/sessions/2026/05"), root, c)); // older month
        assert!(dir_after_cutoff(Path::new("/sessions/2026/06/11"), root, c)); // newer day
        assert!(!dir_after_cutoff(
            Path::new("/sessions/2026/06/09"),
            root,
            c
        )); // older day
    }

    #[test]
    fn scan_extracts_meta_and_fingerprint() {
        let (cwd, id, n, first, _last) = scan(&fixture_path());
        assert_eq!(cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(id.as_deref(), Some("abc123"));
        assert_eq!(n, 1);
        assert_eq!(first.as_deref(), Some("hello codex"));
    }
}
