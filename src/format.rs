use crate::model::{SessionRef, Turn};
use chrono::{DateTime, Local};
use std::time::SystemTime;

pub fn time_short(t: SystemTime) -> String {
    let dt: DateTime<Local> = t.into();
    dt.format("%m-%d %H:%M:%S").to_string()
}

/// Render the cross-agent roster table.
pub fn roster(sessions: &[SessionRef]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<8} {:<36} {:<17} {:<24} {:>3}  PROMPTS\n",
        "AGENT", "SESSION", "MODIFIED", "BRANCH", "#"
    ));
    for s in sessions {
        let branch = s.branch.clone().unwrap_or_else(|| "-".into());
        let branch = truncate(&branch, 24);
        out.push_str(&format!(
            "{:<8} {:<36} {:<17} {:<24} {:>3}  first: {}\n",
            s.agent.label(),
            truncate(&s.id, 36),
            time_short(s.modified),
            branch,
            s.prompts,
            s.first.clone().unwrap_or_else(|| "(none)".into()),
        ));
        if let Some(last) = &s.last {
            if Some(last) != s.first.as_ref() {
                out.push_str(&format!("{:<92} last : {}\n", "", last));
            }
        }
    }
    out
}

/// Render a transcript as Markdown.
pub fn markdown(session: &SessionRef, turns: &[Turn]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Session transcript\n_{} · {} · {}_\n",
        session.agent.label(),
        session.id,
        session.cwd
    ));
    for t in turns {
        out.push_str(&format!("\n## {}\n\n{}\n", t.role.label(), t.text));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect()
    } else {
        s.to_string()
    }
}
