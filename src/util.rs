use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The store directory for exported transcripts: `~/.agent-handoff/` (created if missing).
pub fn store_dir() -> PathBuf {
    let dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agent-handoff");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Open a path with the OS default handler (macOS `open`, Linux `xdg-open`, Windows `start`).
pub fn open_path(path: &Path) -> std::io::Result<()> {
    let openers: [(&str, &[&str]); 3] = [
        ("open", &[]),
        ("xdg-open", &[]),
        ("cmd", &["/C", "start", ""]),
    ];
    for (cmd, args) in openers {
        let mut c = Command::new(cmd);
        c.args(args)
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if c.spawn().is_ok() {
            return Ok(());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no opener found (open/xdg-open/start)",
    ))
}

/// Copy text to the system clipboard. Tries macOS `pbcopy`, then Wayland/X11/Windows tools.
pub fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    let candidates: [(&str, &[&str]); 4] = [
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("clip.exe", &[]),
    ];
    for (cmd, args) in candidates {
        match Command::new(cmd).args(args).stdin(Stdio::piped()).spawn() {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(text.as_bytes())?;
                }
                child.wait()?;
                return Ok(());
            }
            Err(_) => continue, // tool not present — try the next
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no clipboard tool found (pbcopy/wl-copy/xclip/clip.exe)",
    ))
}

/// Parse a human duration like `30s`, `5m`, `2h`, `3d` into a `Duration`.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic())?);
    let n: u64 = num.trim().parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// Claude Code / Cursor-style slug: absolute path with separators replaced.
/// Claude replaces both `/` and `.`; Cursor replaces only `/` and drops the leading dash.
pub fn slug_claude(path: &Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    abs.to_string_lossy().replace('/', "-").replace('.', "-")
}

/// Cursor stores projects under e.g. `Users-livio-Documents-ship-fast` (no leading dash, `/`->`-`).
pub fn slug_cursor(path: &Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    abs.to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-")
}

/// Collapse whitespace and truncate a prompt to a short single-line fingerprint.
pub fn fingerprint_line(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max {
        one.chars().take(max).collect()
    } else {
        one
    }
}

/// Normalize Cursor's wrapped prompt text so it reads like the human prompt:
/// drop `<timestamp>…</timestamp>` entirely, unwrap `<user_query>…</user_query>` (keep inner text).
pub fn strip_wrappers(s: &str) -> String {
    let dropped = transform_tag(s, "<timestamp>", "</timestamp>", false);
    let unwrapped = transform_tag(&dropped, "<user_query>", "</user_query>", true);
    unwrapped.trim().to_string()
}

/// Process every `open…close` span: when `keep_inner` keep the inner content, else drop the span.
fn transform_tag(s: &str, open: &str, close: &str, keep_inner: bool) -> String {
    let mut result = String::new();
    let mut rest = s;
    while let Some(start) = rest.find(open) {
        result.push_str(&rest[..start]);
        let after = &rest[start + open.len()..];
        if let Some(end) = after.find(close) {
            if keep_inner {
                result.push_str(&after[..end]);
            }
            rest = &after[end + close.len()..];
        } else {
            if keep_inner {
                result.push_str(after);
            }
            rest = "";
        }
    }
    result.push_str(rest);
    result
}

/// True if a string looks like a noisy non-conversational system/tool-result payload we should
/// not treat as a real user prompt for fingerprinting.
pub fn looks_like_noise(s: &str) -> bool {
    let t = s.trim_start();
    t.is_empty()
        || t.starts_with("[Request interrupted")
        || t.starts_with("<task-notification")
        || t.starts_with("<local-command")
        || t.starts_with("Caveat:")
        || t.contains("tool_result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("3d"), Some(Duration::from_secs(259200)));
        assert_eq!(parse_duration("12"), None);
        assert_eq!(parse_duration("m"), None);
        assert_eq!(parse_duration("5w"), None);
    }

    #[test]
    fn strip_wrappers_drops_timestamp_keeps_query() {
        let s = "<timestamp>now</timestamp>\n<user_query>do x</user_query>";
        assert_eq!(strip_wrappers(s), "do x");
    }
}

/// Lazily resolve the git branch for a cwd (used by agents that don't record it).
pub fn git_branch(cwd: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if b.is_empty() {
        None
    } else {
        Some(b)
    }
}
