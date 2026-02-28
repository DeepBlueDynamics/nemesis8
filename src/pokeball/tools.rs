use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

use super::protocol::ToolOutput;

/// Execute a tool call and return its output.
/// All tools operate within the /work directory.
pub fn execute_tool(name: &str, input: &Value, tool_call_id: &str) -> ToolOutput {
    let result = match name {
        "bash" => exec_bash(input),
        "file_read" => exec_file_read(input),
        "file_write" => exec_file_write(input),
        "grep" => exec_grep(input),
        "glob" => exec_glob(input),
        _ => Err(anyhow::anyhow!("unknown tool: {name}")),
    };

    match result {
        Ok(output) => ToolOutput {
            tool_call_id: tool_call_id.to_string(),
            success: true,
            output,
            error: None,
        },
        Err(e) => ToolOutput {
            tool_call_id: tool_call_id.to_string(),
            success: false,
            output: String::new(),
            error: Some(e.to_string()),
        },
    }
}

/// Execute a bash command in /work
fn exec_bash(input: &Value) -> Result<String> {
    let command = input
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("bash: missing 'command' field"))?;

    let _timeout_ms = input
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(120_000);

    let output = Command::new("bash")
        .args(["-c", command])
        .current_dir("/work")
        .output()
        .map_err(|e| anyhow::anyhow!("bash exec error: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("STDERR: ");
        result.push_str(&stderr);
    }

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        anyhow::bail!("exit code {code}\n{result}");
    }

    Ok(result)
}

/// Read a file from /work
fn exec_file_read(input: &Value) -> Result<String> {
    let file_path = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("file_read: missing 'file_path' field"))?;

    // Resolve relative to /work
    let path = resolve_path(file_path);

    let offset = input
        .get("offset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(2000) as usize;

    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;

    let lines: Vec<&str> = content.lines().collect();
    let end = (offset + limit).min(lines.len());

    if offset >= lines.len() {
        return Ok(String::new());
    }

    let mut result = String::new();
    for (i, line) in lines[offset..end].iter().enumerate() {
        result.push_str(&format!("{:>6}\t{}\n", offset + i + 1, line));
    }

    Ok(result)
}

/// Write a file in /work
fn exec_file_write(input: &Value) -> Result<String> {
    let file_path = input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("file_write: missing 'file_path' field"))?;

    let content = input
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("file_write: missing 'content' field"))?;

    let path = resolve_path(file_path);

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;

    Ok(format!("wrote {} bytes to {}", content.len(), path.display()))
}

/// Search file contents with grep
fn exec_grep(input: &Value) -> Result<String> {
    let pattern = input
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("grep: missing 'pattern' field"))?;

    let search_path = input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/work");

    let path = resolve_path(search_path);

    let mut cmd = Command::new("grep");
    cmd.args(["-rn", "--include=*", pattern])
        .arg(path.to_str().unwrap_or("/work"))
        .current_dir("/work");

    if let Some(glob) = input.get("glob").and_then(|v| v.as_str()) {
        cmd.arg(format!("--include={glob}"));
    }

    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("grep exec error: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Truncate long output
    let max_lines = 100;
    let lines: Vec<&str> = stdout.lines().take(max_lines).collect();
    let mut result = lines.join("\n");
    if stdout.lines().count() > max_lines {
        result.push_str(&format!("\n... (truncated, {} total matches)", stdout.lines().count()));
    }

    Ok(result)
}

/// Glob for files matching a pattern
fn exec_glob(input: &Value) -> Result<String> {
    let pattern = input
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("glob: missing 'pattern' field"))?;

    let search_path = input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/work");

    let path = resolve_path(search_path);

    // Use find + grep as a simple glob implementation
    let output = Command::new("find")
        .arg(path.to_str().unwrap_or("/work"))
        .args(["-name", pattern, "-type", "f"])
        .output()
        .map_err(|e| anyhow::anyhow!("glob exec error: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Truncate
    let max_files = 200;
    let lines: Vec<&str> = stdout.lines().take(max_files).collect();
    let mut result = lines.join("\n");
    if stdout.lines().count() > max_files {
        result.push_str(&format!("\n... ({} total files)", stdout.lines().count()));
    }

    Ok(result)
}

/// Resolve a path relative to /work, preventing escape.
/// Uses string-based logic since this targets Linux containers.
fn resolve_path(file_path: &str) -> std::path::PathBuf {
    if file_path.starts_with('/') {
        // Absolute path — allow /work, /comms, /tmp; jail everything else
        if file_path.starts_with("/work")
            || file_path.starts_with("/comms")
            || file_path.starts_with("/tmp")
        {
            Path::new(file_path).to_path_buf()
        } else {
            // Jail under /work
            let stripped = file_path.trim_start_matches('/');
            Path::new("/work").join(stripped)
        }
    } else {
        Path::new("/work").join(file_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_path_relative() {
        let p = resolve_path("src/main.rs");
        assert_eq!(p, Path::new("/work/src/main.rs"));
    }

    #[test]
    fn test_resolve_path_absolute_work() {
        let p = resolve_path("/work/src/main.rs");
        assert_eq!(p, Path::new("/work/src/main.rs"));
    }

    #[test]
    fn test_resolve_path_absolute_escape() {
        let p = resolve_path("/etc/passwd");
        assert_eq!(p, Path::new("/work/etc/passwd"));
    }

    #[test]
    fn test_resolve_path_comms() {
        let p = resolve_path("/comms/inbox/test.json");
        assert_eq!(p, Path::new("/comms/inbox/test.json"));
    }

    #[test]
    fn test_execute_unknown_tool() {
        let output = execute_tool(
            "unknown_tool",
            &serde_json::json!({}),
            "tc-1",
        );
        assert!(!output.success);
        assert!(output.error.unwrap().contains("unknown tool"));
    }
}
