use super::{home, Adapter, Query};
use crate::model::{Agent, Role, SessionRef, Turn};
use crate::util::{fingerprint_line, git_branch, looks_like_noise};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

pub struct DevinAdapter;

fn db_path() -> PathBuf {
    home().join(".local/share/devin/cli/sessions.db")
}

/// Walk the main chain from the leaf recorded in `sessions.main_chain_id` up to the root,
/// then return the nodes ordered from root to leaf.
fn main_chain(conn: &Connection, session_id: &str) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE chain(node_id, parent_node_id, chat_message, created_at, depth) AS ( \
         SELECT node_id, parent_node_id, chat_message, created_at, 1 \
         FROM message_nodes \
         WHERE session_id = ?1 \
           AND node_id = (SELECT main_chain_id FROM sessions WHERE id = ?1) \
         UNION ALL \
         SELECT m.node_id, m.parent_node_id, m.chat_message, m.created_at, c.depth + 1 \
         FROM message_nodes m \
         JOIN chain c ON m.node_id = c.parent_node_id \
         WHERE m.session_id = ?1 \
         ) \
         SELECT chat_message FROM chain ORDER BY depth DESC",
    )?;
    let rows = stmt.query_map([session_id], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for raw in rows.flatten() {
        if let Ok(v) = serde_json::from_str(&raw) {
            out.push(v);
        }
    }
    Ok(out)
}

/// Extract user/assistant turns from the main chain.
fn turns_for(messages: &[Value], include_tools: bool) -> Vec<Turn> {
    let mut turns = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        let content = msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match role {
            "system" => continue,
            "user" => {
                if !content.trim().is_empty() {
                    turns.push(Turn {
                        role: Role::User,
                        text: content,
                    });
                }
            }
            "assistant" => {
                if !content.trim().is_empty() {
                    turns.push(Turn {
                        role: Role::Assistant,
                        text: content,
                    });
                }
                if include_tools {
                    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            let name = call.get("name").and_then(Value::as_str).unwrap_or("?");
                            let args = call
                                .get("arguments")
                                .map(|v| v.to_string())
                                .unwrap_or_default();
                            turns.push(Turn {
                                role: Role::Assistant,
                                text: format!("`[tool: {}]` {}", name, truncate(&args, 500)),
                            });
                        }
                    }
                }
            }
            "tool" if include_tools => {
                let id = msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                turns.push(Turn {
                    role: Role::Assistant,
                    text: format!("`[tool result: {}]` {}", id, truncate(&content, 1000)),
                });
            }
            _ => {}
        }
    }
    turns
}

fn fingerprint(messages: &[Value]) -> (usize, Option<String>, Option<String>) {
    let mut n = 0usize;
    let mut first = None;
    let mut last = None;
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = msg.get("content").and_then(Value::as_str).unwrap_or("");
        if text.trim().is_empty() || looks_like_noise(text) {
            continue;
        }
        n += 1;
        let fp = fingerprint_line(text, 70);
        if first.is_none() {
            first = Some(fp.clone());
        }
        last = Some(fp);
    }
    (n, first, last)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect()
    } else {
        s.to_string()
    }
}

impl Adapter for DevinAdapter {
    fn agent(&self) -> Agent {
        Agent::Devin
    }

    fn discover(&self, q: &Query) -> Result<Vec<SessionRef>> {
        let path = db_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let target = q.cwd.to_string_lossy().to_string();
        let conn = Connection::open(&path)?;
        let mut stmt =
            conn.prepare("SELECT id, working_directory, last_activity_at FROM sessions WHERE hidden = 0 AND working_directory = ?1")?;
        let rows = stmt.query_map([&target], |row| {
            let id: String = row.get(0)?;
            let cwd: String = row.get(1)?;
            let activity: i64 = row.get(2)?;
            Ok((id, cwd, activity))
        })?;

        let branch = git_branch(&target);
        let mut out = Vec::new();
        for (id, cwd, activity) in rows.flatten() {
            let modified = UNIX_EPOCH + Duration::from_secs(activity.max(0) as u64);
            if !q.recent_enough(modified) {
                continue;
            }
            let messages = main_chain(&conn, &id).unwrap_or_default();
            let (prompts, first, last) = fingerprint(&messages);
            out.push(SessionRef {
                agent: Agent::Devin,
                id,
                path: path.clone(),
                cwd,
                branch: branch.clone(),
                modified,
                prompts,
                first,
                last,
            });
        }
        Ok(out)
    }

    fn read(&self, session: &SessionRef, include_tools: bool) -> Result<Vec<Turn>> {
        let conn = Connection::open(db_path())?;
        let messages = main_chain(&conn, &session.id)?;
        Ok(turns_for(&messages, include_tools))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> (Connection, String) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions ( \
             id TEXT PRIMARY KEY, \
             working_directory TEXT NOT NULL, \
             backend_type TEXT NOT NULL, \
             model TEXT NOT NULL, \
             agent_mode TEXT NOT NULL, \
             created_at INTEGER NOT NULL, \
             last_activity_at INTEGER NOT NULL, \
             title TEXT, \
             main_chain_id INTEGER, \
             hidden INTEGER NOT NULL DEFAULT 0); \
             CREATE TABLE message_nodes ( \
             row_id INTEGER PRIMARY KEY AUTOINCREMENT, \
             session_id TEXT NOT NULL, \
             node_id INTEGER NOT NULL, \
             parent_node_id INTEGER, \
             chat_message TEXT NOT NULL, \
             created_at INTEGER NOT NULL, \
             metadata TEXT, \
             UNIQUE(session_id, node_id));",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode, created_at, last_activity_at, title, main_chain_id, hidden) \
             VALUES ('s1', '/tmp/proj', 'devin', 'gpt-4', 'default', 1, 5, 'Test', 3, 0)",
            [],
        )
        .unwrap();
        let msgs = [
            (
                0i64,
                None::<i64>,
                r#"{"role":"system","content":"sys"}"#,
                1i64,
            ),
            (1, Some(0), r#"{"role":"user","content":"hello"}"#, 2),
            (2, Some(1), r#"{"role":"assistant","content":"hi"}"#, 3),
            (
                3,
                Some(2),
                r#"{"role":"user","content":"what about devin"}"#,
                4,
            ),
        ];
        for (nid, parent, msg, ts) in msgs {
            conn.execute(
                "INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message, created_at) \
                 VALUES ('s1', ?1, ?2, ?3, ?4)",
                rusqlite::params![nid, parent, msg, ts],
            )
            .unwrap();
        }
        (conn, "s1".to_string())
    }

    #[test]
    fn main_chain_orders_root_to_leaf() {
        let (conn, sid) = seed();
        let msgs = main_chain(&conn, &sid).unwrap();
        assert_eq!(msgs.len(), 4);
        let roles: Vec<&str> = msgs
            .iter()
            .map(|m| m.get("role").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    }

    #[test]
    fn turns_default_text_only() {
        let (conn, sid) = seed();
        let msgs = main_chain(&conn, &sid).unwrap();
        let turns = turns_for(&msgs, false);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].text, "hello");
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(turns[1].text, "hi");
        assert_eq!(turns[2].role, Role::User);
        assert_eq!(turns[2].text, "what about devin");
    }

    #[test]
    fn fingerprint_counts_prompts() {
        let (conn, sid) = seed();
        let msgs = main_chain(&conn, &sid).unwrap();
        let (n, first, last) = fingerprint(&msgs);
        assert_eq!(n, 2);
        assert_eq!(first.as_deref(), Some("hello"));
        assert_eq!(last.as_deref(), Some("what about devin"));
    }
}
