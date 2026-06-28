use super::{home, Adapter, Query};
use crate::model::{Agent, Role, SessionRef, Turn};
use crate::util::{fingerprint_line, git_branch, looks_like_noise, slug_cursor, strip_wrappers};
use anyhow::Result;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

pub struct CursorAdapter;

fn transcripts_dir(cwd: &Path) -> PathBuf {
    home()
        .join(".cursor/projects")
        .join(slug_cursor(cwd))
        .join("agent-transcripts")
}

fn flatten(content: &Value, include_tools: bool) -> String {
    if let Some(s) = content.as_str() {
        return strip_wrappers(s);
    }
    let mut parts = Vec::new();
    if let Some(arr) = content.as_array() {
        for b in arr {
            match b.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    if let Some(x) = b.get("text").and_then(Value::as_str) {
                        parts.push(strip_wrappers(x));
                    }
                }
                "tool_use" if include_tools => {
                    let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                    parts.push(format!("`[tool: {}]`", name));
                }
                t if include_tools && (t == "tool_result" || t == "tool-result") => {
                    parts.push("`[tool result]`".to_string());
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

impl Adapter for CursorAdapter {
    fn agent(&self) -> Agent {
        Agent::Cursor
    }

    fn discover(&self, q: &Query) -> Result<Vec<SessionRef>> {
        let dir = transcripts_dir(q.cwd);
        let target = q.cwd.to_string_lossy().to_string();
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        let branch = git_branch(&target); // once per cwd
        for entry in WalkDir::new(&dir).into_iter().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // Cheap stat-gate before opening the transcript.
            let modified = fs::metadata(path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if !q.recent_enough(modified) {
                continue;
            }
            let (n, first, last) = scan(path);
            let id = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            out.push(SessionRef {
                agent: Agent::Cursor,
                id,
                path: path.to_path_buf(),
                cwd: target.clone(),
                branch: branch.clone(),
                modified,
                prompts: n,
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
            let role = match o.get("role").and_then(Value::as_str).unwrap_or("") {
                "assistant" => Role::Assistant,
                "user" => Role::User,
                _ => continue,
            };
            let content = o.get("message").and_then(|m| m.get("content"));
            let body = content
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

fn scan(path: &Path) -> (usize, Option<String>, Option<String>) {
    let mut n = 0usize;
    let mut first = None;
    let mut last = None;
    if let Ok(file) = fs::File::open(path) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let o: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if o.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let content = o.get("message").and_then(|m| m.get("content"));
            let text = content.map(|c| flatten(c, false)).unwrap_or_default();
            if !text.trim().is_empty() && !looks_like_noise(&text) {
                n += 1;
                let fp = fingerprint_line(&text, 70);
                if first.is_none() {
                    first = Some(fp.clone());
                }
                last = Some(fp);
            }
        }
    }
    (n, first, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> SessionRef {
        SessionRef {
            agent: Agent::Cursor,
            id: "sample".into(),
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/cursor/sample.jsonl"),
            cwd: "/tmp/proj".into(),
            branch: None,
            modified: SystemTime::UNIX_EPOCH,
            prompts: 0,
            first: None,
            last: None,
        }
    }

    #[test]
    fn read_strips_wrappers_and_skips_tools() {
        let turns = CursorAdapter.read(&fixture(), false).unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].text, "do the thing"); // <timestamp>/<user_query> stripped
        assert_eq!(turns[1].text, "doing the thing"); // tool_use dropped when tools off
        assert_eq!(turns[2].text, "second query");
    }

    #[test]
    fn read_with_tools_marks_tool_use() {
        let turns = CursorAdapter.read(&fixture(), true).unwrap();
        let joined: String = turns
            .iter()
            .map(|t| t.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[tool: Read]"));
    }

    #[test]
    fn scan_counts_user_prompts() {
        let (n, first, last) = scan(&fixture().path);
        assert_eq!(n, 2);
        assert_eq!(first.as_deref(), Some("do the thing"));
        assert_eq!(last.as_deref(), Some("second query"));
    }
}
