use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Session metadata
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub path: String,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub size_bytes: u64,
    pub line_count: usize,
    pub workspace: Option<String>,
}

/// List all sessions from the given directories
pub fn list_sessions(session_dirs: &[&str]) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();

    for dir in session_dirs {
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            continue;
        }
        collect_sessions(dir_path, &mut sessions)?;
    }

    // Sort by modified time (newest first)
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));

    Ok(sessions)
}

/// Find a session by ID (full UUID or last 5 characters)
pub fn find_session(id: &str, session_dirs: &[&str]) -> Result<Option<SessionInfo>> {
    let all = list_sessions(session_dirs)?;

    // Try exact match first
    if let Some(s) = all.iter().find(|s| s.id == id) {
        return Ok(Some(s.clone()));
    }

    // Try suffix match (last N chars)
    let suffix = id.to_lowercase();
    let matches: Vec<_> = all
        .iter()
        .filter(|s| s.id.to_lowercase().ends_with(&suffix))
        .collect();

    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0].clone())),
        n => {
            tracing::warn!(
                matches = n,
                suffix = %suffix,
                "ambiguous session ID suffix, returning most recent"
            );
            Ok(Some(matches[0].clone()))
        }
    }
}

/// Recursively collect .jsonl session files from a directory
fn collect_sessions(dir: &Path, sessions: &mut Vec<SessionInfo>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading session dir {}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sessions(&path, sessions)?;
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            if let Some(info) = parse_session_file(&path) {
                sessions.push(info);
            }
        }
    }

    Ok(())
}

/// Extract session info from a .jsonl file path
fn parse_session_file(path: &Path) -> Option<SessionInfo> {
    let filename = path.file_name()?.to_str()?;

    // Session files are named like:
    // rollout-2026-02-21T00-02-09-019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl
    let session_id = extract_session_id(filename)?;

    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        });
    let created = metadata
        .created()
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        });

    let size_bytes = metadata.len();

    // Count lines without reading entire file into memory
    let line_count = count_lines(path).unwrap_or(0);

    // Read workspace from session_meta in the first line
    let workspace = read_session_workspace(path);

    Some(SessionInfo {
        id: session_id,
        path: path.to_string_lossy().to_string(),
        created,
        modified,
        size_bytes,
        line_count,
        workspace,
    })
}

/// Extract UUID from a session filename
fn extract_session_id(filename: &str) -> Option<String> {
    // Try to find a UUID pattern (8-4-4-4-12 hex)
    let stripped = filename.trim_end_matches(".jsonl");

    // Walk from the end looking for the UUID
    // UUID format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx (36 chars)
    if stripped.len() >= 36 {
        let candidate = &stripped[stripped.len() - 36..];
        if is_uuid_format(candidate) {
            return Some(candidate.to_string());
        }
    }

    // Fallback: use the whole filename stem as the ID
    Some(stripped.to_string())
}

fn is_uuid_format(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected_lens = [8, 4, 4, 4, 12];
    for (part, &expected) in parts.iter().zip(&expected_lens) {
        if part.len() != expected || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    true
}

/// Read workspace path from session file — checks for host path in early lines,
/// falls back to cwd from session_meta
fn read_session_workspace(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut cwd = None;

    // Read first few lines looking for workspace info
    for (i, line) in reader.lines().enumerate() {
        if i > 10 { break; } // only check first 10 lines
        let line = line.ok()?;

        // Look for NEMESIS8_HOST_WORKSPACE in any line
        if line.contains("NEMESIS8_HOST_WORKSPACE") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                // Could be in env vars or message content
                let text = v.to_string();
                if let Some(start) = text.find("NEMESIS8_HOST_WORKSPACE=") {
                    let rest = &text[start + 24..];
                    let end = rest.find(|c: char| c == '"' || c == ',' || c == '}').unwrap_or(rest.len());
                    return Some(rest[..end].to_string());
                }
            }
        }

        // First line has session_meta with cwd
        if i == 0 {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                cwd = v.get("payload")
                    .and_then(|p| p.get("cwd"))
                    .and_then(|c| c.as_str())
                    .map(|s| s.to_string());
            }
        }
    }

    cwd
}

fn count_lines(path: &Path) -> Result<usize> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    Ok(reader.lines().count())
}

/// Print sessions in a human-readable table format
pub fn print_sessions(sessions: &[SessionInfo]) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }

    println!(
        "{:<38}  {:<24}  {:>8}  {:>6}  {}",
        "SESSION ID", "MODIFIED", "SIZE", "LINES", "WORKSPACE"
    );
    println!("{}", "-".repeat(100));

    for s in sessions.iter().take(20) {
        let modified = s
            .modified
            .as_deref()
            .map(|m| &m[..19])
            .unwrap_or("unknown");
        let size = format_size(s.size_bytes);
        let ws = s.workspace.as_deref().unwrap_or("");
        println!(
            "{:<38}  {:<24}  {:>8}  {:>6}  {}",
            s.id, modified, size, s.line_count, ws
        );
    }

    if sessions.len() > 20 {
        println!("  ... and {} more", sessions.len() - 20);
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uuid_format() {
        assert!(is_uuid_format("019c7d80-f629-7452-b38c-ac4ab228d44d"));
        assert!(!is_uuid_format("not-a-uuid"));
        assert!(!is_uuid_format("too-short"));
    }

    #[test]
    fn test_uuid_format_edge_cases() {
        // Empty string
        assert!(!is_uuid_format(""));
        // Right length but wrong separators
        assert!(!is_uuid_format("019c7d80_f629_7452_b38c_ac4ab228d44d"));
        // Right format but non-hex characters
        assert!(!is_uuid_format("019c7d80-f629-7452-b38c-zz4ab228d44d"));
        // All zeros is valid
        assert!(is_uuid_format("00000000-0000-0000-0000-000000000000"));
        // All f's is valid
        assert!(is_uuid_format("ffffffff-ffff-ffff-ffff-ffffffffffff"));
    }

    #[test]
    fn test_extract_session_id() {
        let id = extract_session_id(
            "rollout-2026-02-21T00-02-09-019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl",
        );
        assert_eq!(id.unwrap(), "019c7d80-f629-7452-b38c-ac4ab228d44d");
    }

    #[test]
    fn test_extract_session_id_bare_uuid() {
        let id = extract_session_id("019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl");
        assert_eq!(id.unwrap(), "019c7d80-f629-7452-b38c-ac4ab228d44d");
    }

    #[test]
    fn test_extract_session_id_no_uuid_fallback() {
        let id = extract_session_id("some-random-file.jsonl");
        // Falls back to the full stem
        assert_eq!(id.unwrap(), "some-random-file");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1048576), "1.0 MB");
        assert_eq!(format_size(1572864), "1.5 MB");
    }

    #[test]
    fn test_list_sessions_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_str().unwrap();
        let sessions = list_sessions(&[dir_str]).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_list_sessions_nonexistent_dir() {
        let sessions = list_sessions(&["/nonexistent/path"]).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_list_sessions_with_files() {
        let dir = tempfile::tempdir().unwrap();

        // Create session files
        let f1 = "rollout-2026-02-21T00-02-09-019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl";
        let f2 = "rollout-2026-02-22T10-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl";

        std::fs::write(
            dir.path().join(f1),
            "{\"type\":\"message\"}\n{\"type\":\"tool\"}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(f2), "{\"type\":\"message\"}\n").unwrap();

        let dir_str = dir.path().to_str().unwrap();
        let sessions = list_sessions(&[dir_str]).unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn test_list_sessions_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("2026/02/21");
        std::fs::create_dir_all(&sub).unwrap();

        std::fs::write(
            sub.join("rollout-019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl"),
            "{}\n",
        )
        .unwrap();

        let dir_str = dir.path().to_str().unwrap();
        let sessions = list_sessions(&[dir_str]).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "019c7d80-f629-7452-b38c-ac4ab228d44d");
    }

    #[test]
    fn test_find_session_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rollout-019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl"),
            "{}\n",
        )
        .unwrap();

        let dir_str = dir.path().to_str().unwrap();
        let found =
            find_session("019c7d80-f629-7452-b38c-ac4ab228d44d", &[dir_str]).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "019c7d80-f629-7452-b38c-ac4ab228d44d");
    }

    #[test]
    fn test_find_session_by_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("rollout-019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl"),
            "{}\n",
        )
        .unwrap();

        let dir_str = dir.path().to_str().unwrap();
        let found = find_session("8d44d", &[dir_str]).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "019c7d80-f629-7452-b38c-ac4ab228d44d");
    }

    #[test]
    fn test_find_session_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_str().unwrap();
        let found = find_session("nonexistent", &[dir_str]).unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_session_line_count() {
        let dir = tempfile::tempdir().unwrap();
        let content = "{\"type\":\"a\"}\n{\"type\":\"b\"}\n{\"type\":\"c\"}\n";
        std::fs::write(
            dir.path().join("rollout-019c7d80-f629-7452-b38c-ac4ab228d44d.jsonl"),
            content,
        )
        .unwrap();

        let dir_str = dir.path().to_str().unwrap();
        let sessions = list_sessions(&[dir_str]).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].line_count, 3);
    }

    #[test]
    fn test_multiple_session_dirs() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        std::fs::write(
            dir1.path().join("rollout-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"),
            "{}\n",
        )
        .unwrap();
        std::fs::write(
            dir2.path().join("rollout-11111111-2222-3333-4444-555555555555.jsonl"),
            "{}\n",
        )
        .unwrap();

        let d1 = dir1.path().to_str().unwrap();
        let d2 = dir2.path().to_str().unwrap();
        let sessions = list_sessions(&[d1, d2]).unwrap();
        assert_eq!(sessions.len(), 2);
    }
}
