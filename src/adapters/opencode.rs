use super::{home, Adapter, Query};
use crate::model::{Agent, Role, SessionRef, Turn};
use crate::util::{fingerprint_line, git_branch, looks_like_noise};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

pub struct OpenCodeAdapter;

fn db_path() -> PathBuf {
    home().join(".local/share/opencode/opencode.db")
}

/// Pull ordered (role, text) pairs for a session from message+part joins.
fn turns_for(conn: &Connection, session_id: &str, include_tools: bool) -> Result<Vec<Turn>> {
    let mut stmt = conn.prepare(
        "SELECT m.data, p.data FROM message m \
         JOIN part p ON p.message_id = m.id \
         WHERE m.session_id = ?1 \
         ORDER BY m.time_created, p.time_created",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        let m: String = row.get(0)?;
        let p: String = row.get(1)?;
        Ok((m, p))
    })?;
    let mut turns = Vec::new();
    for r in rows.flatten() {
        let (mdata, pdata) = r;
        let mj: Value = serde_json::from_str(&mdata).unwrap_or(Value::Null);
        let pj: Value = serde_json::from_str(&pdata).unwrap_or(Value::Null);
        let role = match mj.get("role").and_then(Value::as_str).unwrap_or("") {
            "assistant" => Role::Assistant,
            "user" => Role::User,
            _ => continue,
        };
        let ptype = pj.get("type").and_then(Value::as_str).unwrap_or("");
        let text = match ptype {
            "text" => pj
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            other if include_tools => format!("`[{}]`", other),
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        turns.push(Turn { role, text });
    }
    Ok(turns)
}

impl Adapter for OpenCodeAdapter {
    fn agent(&self) -> Agent {
        Agent::OpenCode
    }

    fn discover(&self, q: &Query) -> Result<Vec<SessionRef>> {
        let path = db_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let target = q.cwd.to_string_lossy().to_string();
        let conn = Connection::open(&path)?;
        let mut stmt = conn.prepare("SELECT id FROM session WHERE directory = ?1")?;
        let ids: Vec<String> = stmt
            .query_map([&target], |row| row.get::<_, String>(0))?
            .flatten()
            .collect();

        let branch = git_branch(&target); // once per cwd
        let mut out = Vec::new();
        for id in ids {
            // Cheap gate first: a single MAX(time_created) query, before reading message parts.
            let modified: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(time_created),0) FROM message WHERE session_id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let modified_t = UNIX_EPOCH + Duration::from_millis(modified.max(0) as u64);
            if !q.recent_enough(modified_t) {
                continue;
            }
            let turns = turns_for(&conn, &id, false)?;
            let mut first = None;
            let mut last = None;
            let mut n = 0;
            for t in &turns {
                if t.role == Role::User && !looks_like_noise(&t.text) {
                    n += 1;
                    let fp = fingerprint_line(&t.text, 70);
                    if first.is_none() {
                        first = Some(fp.clone());
                    }
                    last = Some(fp);
                }
            }
            out.push(SessionRef {
                agent: Agent::OpenCode,
                id,
                path: path.clone(),
                cwd: target.clone(),
                branch: branch.clone(),
                modified: modified_t,
                prompts: n,
                first,
                last,
            });
        }
        Ok(out)
    }

    fn read(&self, session: &SessionRef, include_tools: bool) -> Result<Vec<Turn>> {
        let conn = Connection::open(db_path())?;
        turns_for(&conn, &session.id, include_tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
             INSERT INTO message VALUES ('m1','s1',1,'{\"role\":\"user\"}');
             INSERT INTO part    VALUES ('p1','m1','s1',1,'{\"type\":\"text\",\"text\":\"hello\"}');
             INSERT INTO message VALUES ('m2','s1',2,'{\"role\":\"assistant\"}');
             INSERT INTO part    VALUES ('p2','m2','s1',2,'{\"type\":\"text\",\"text\":\"hi back\"}');
             INSERT INTO part    VALUES ('p3','m2','s1',3,'{\"type\":\"tool\",\"name\":\"bash\"}');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn turns_default_text_only_in_order() {
        let conn = seed();
        let turns = turns_for(&conn, "s1", false).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].text, "hello");
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(turns[1].text, "hi back");
    }

    #[test]
    fn turns_with_tools_includes_nontext_parts() {
        let conn = seed();
        let turns = turns_for(&conn, "s1", true).unwrap();
        let joined: String = turns
            .iter()
            .map(|t| t.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[tool]"));
    }
}
