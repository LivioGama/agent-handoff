use super::{home, Adapter, Query};
use crate::model::{Agent, Role, SessionRef, Turn};
use crate::util::{fingerprint_line, git_branch, looks_like_noise, slug_claude};
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct ClaudeAdapter;

fn project_dir(cwd: &Path) -> PathBuf {
    home().join(".claude/projects").join(slug_claude(cwd))
}

/// Flatten Claude's `message.content` (string | array of typed blocks) to readable text.
fn flatten(content: &Value, include_tools: bool) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let mut parts = Vec::new();
    if let Some(arr) = content.as_array() {
        for b in arr {
            let t = b.get("type").and_then(Value::as_str).unwrap_or("");
            match t {
                "text" => {
                    if let Some(x) = b.get("text").and_then(Value::as_str) {
                        parts.push(x.to_string());
                    }
                }
                "thinking" => {
                    if let Some(x) = b.get("thinking").and_then(Value::as_str) {
                        parts.push(format!("> [thinking]\n> {}", x.replace('\n', "\n> ")));
                    }
                }
                "tool_use" if include_tools => {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                    let input = b.get("input").map(|v| v.to_string()).unwrap_or_default();
                    parts.push(format!("`[tool: {}]` {}", name, truncate(&input, 500)));
                }
                "tool_result" if include_tools => {
                    let c = b.get("content");
                    let text = match c {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(a)) => a
                            .iter()
                            .filter_map(|x| x.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join(" "),
                        _ => String::new(),
                    };
                    parts.push(format!("`[tool result]` {}", truncate(&text, 1000)));
                }
                _ => {}
            }
        }
    }
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect()
    } else {
        s.to_string()
    }
}

impl Adapter for ClaudeAdapter {
    fn agent(&self) -> Agent {
        Agent::ClaudeCode
    }

    fn discover(&self, q: &Query) -> Result<Vec<SessionRef>> {
        let dir = project_dir(q.cwd);
        let mut out = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(out),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // Cheap stat-gate: skip stale files without opening them.
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if !q.recent_enough(modified) {
                continue;
            }
            let (cwd_field, branch, prompts, first, last) = scan(&path);
            let id = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            out.push(SessionRef {
                agent: Agent::ClaudeCode,
                id,
                path,
                cwd: cwd_field.unwrap_or_else(|| q.cwd.to_string_lossy().to_string()),
                branch,
                modified,
                prompts,
                first,
                last,
            });
        }
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
            let t = o.get("type").and_then(Value::as_str).unwrap_or("");
            if t != "user" && t != "assistant" {
                continue;
            }
            let msg = match o.get("message") {
                Some(m) => m,
                None => continue,
            };
            let role = match msg.get("role").and_then(Value::as_str).unwrap_or(t) {
                "assistant" => Role::Assistant,
                _ => Role::User,
            };
            let body = msg
                .get("content")
                .map(|c| flatten(c, include_tools))
                .unwrap_or_default();
            if body.trim().is_empty() {
                continue;
            }
            turns.push(Turn { role, text: body });
        }
        Ok(turns)
    }
}

/// Cheap fingerprint pass: returns (cwd, branch, #user-prompts, first, last).
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
    let mut branch = None;
    let mut n = 0usize;
    let mut first = None;
    let mut last = None;
    if let Ok(file) = fs::File::open(path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let o: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(b) = o.get("gitBranch").and_then(Value::as_str) {
                if !b.is_empty() {
                    branch = Some(b.to_string());
                }
            }
            if let Some(c) = o.get("cwd").and_then(Value::as_str) {
                cwd = Some(c.to_string());
            }
            if o.get("type").and_then(Value::as_str) == Some("user") {
                if let Some(s) = o
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                {
                    if !looks_like_noise(s) {
                        n += 1;
                        let fp = fingerprint_line(s, 70);
                        if first.is_none() {
                            first = Some(fp.clone());
                        }
                        last = Some(fp);
                    }
                }
            }
        }
    }
    if branch.is_none() {
        if let Some(ref c) = cwd {
            branch = git_branch(c);
        }
    }
    (cwd, branch, n, first, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> SessionRef {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/sample.jsonl");
        SessionRef {
            agent: Agent::ClaudeCode,
            id: "sample".into(),
            path,
            cwd: "/tmp/proj".into(),
            branch: None,
            modified: SystemTime::UNIX_EPOCH,
            prompts: 0,
            first: None,
            last: None,
        }
    }

    #[test]
    fn read_default_skips_tools_keeps_thinking() {
        let turns = ClaudeAdapter.read(&fixture(), false).unwrap();
        // 2 user prompts + 1 assistant text/thinking (the tool_use-only turn is dropped)
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].text, "first prompt here");
        assert!(turns[1].text.contains("answer one"));
        assert!(turns[1].text.contains("[thinking]"));
        assert_eq!(turns[2].text, "second prompt here");
    }

    #[test]
    fn read_with_tools_includes_tool_blocks() {
        let turns = ClaudeAdapter.read(&fixture(), true).unwrap();
        let joined: String = turns
            .iter()
            .map(|t| t.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[tool: Bash]"));
        assert!(joined.contains("[tool result]"));
    }

    #[test]
    fn scan_fingerprints_prompts_and_branch() {
        let path = fixture().path;
        let (cwd, branch, n, first, last) = scan(&path);
        assert_eq!(cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(branch.as_deref(), Some("main"));
        assert_eq!(n, 2);
        assert_eq!(first.as_deref(), Some("first prompt here"));
        assert_eq!(last.as_deref(), Some("second prompt here"));
    }
}
