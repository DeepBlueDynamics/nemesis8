//! What to do with a container once its agent has exited.
//!
//! The choice is made INSIDE the container (the entry shows a menu on the
//! agent's TTY) but acted on by the HOST (`spawn_detached_and_attach`, which
//! owns `docker rm`). The two sides meet through a small file under the shared
//! data home: `<home>/.n8/exit-choices/<agent_id>`, first line the choice,
//! optional second line the session id, so the host can print an exact
//! `n8 resume <id>`. The host deletes the file after reading it.
//!
//! Detach never reaches the host through this file: the container simply keeps
//! running and the user leaves with the detach key.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitChoice {
    /// Delete the container from Docker. The session stays on disk.
    Remove,
    /// Leave the exited container in Docker for `n8 attach` / `n8 resume`.
    Stop,
    /// Keep the container running in the background.
    Detach,
}

impl ExitChoice {
    fn as_str(self) -> &'static str {
        match self {
            ExitChoice::Remove => "remove",
            ExitChoice::Stop => "stop",
            ExitChoice::Detach => "detach",
        }
    }
}

/// Parse a menu answer: `r`/`remove`, `s`/`stop`, `d`/`detach` in any case,
/// surrounding whitespace ignored. An empty answer is the default, Stop.
/// Anything else is `None` (ask again).
pub fn parse_choice(input: &str) -> Option<ExitChoice> {
    let s = input.trim().to_ascii_lowercase();
    match s.as_str() {
        "" | "s" | "stop" => Some(ExitChoice::Stop),
        "r" | "rm" | "remove" => Some(ExitChoice::Remove),
        "d" | "detach" => Some(ExitChoice::Detach),
        _ => None,
    }
}

pub fn choice_path(data_home: &Path, agent_id: &str) -> PathBuf {
    data_home.join(".n8").join("exit-choices").join(agent_id)
}

/// Record the choice for the host. Best-effort: a failed write means the host
/// falls back to its legacy behaviour (remove the exited container).
pub fn write_choice(data_home: &Path, agent_id: &str, choice: ExitChoice, session_id: Option<&str>) {
    let path = choice_path(data_home, agent_id);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut body = choice.as_str().to_string();
    if let Some(sid) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
        body.push('\n');
        body.push_str(sid);
    }
    let _ = std::fs::write(path, body);
}

/// Read and delete the recorded choice: `(choice, session_id)`. `None` when
/// the container never wrote one (an older image, or a crash).
pub fn take_choice(data_home: &Path, agent_id: &str) -> Option<(ExitChoice, Option<String>)> {
    let path = choice_path(data_home, agent_id);
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let mut lines = text.lines();
    let choice = parse_choice(lines.next().unwrap_or(""))?;
    let sid = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some((choice, sid))
}

/// The one-line hint for getting a session back, used by both sides' messages.
pub fn resume_hint(session_id: Option<&str>) -> String {
    match session_id {
        Some(sid) => format!("n8 resume {sid}"),
        None => "n8 sessions   (then n8 resume <id>)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse_case_insensitively_and_default_to_stop() {
        assert_eq!(parse_choice(""), Some(ExitChoice::Stop));
        assert_eq!(parse_choice("  \n"), Some(ExitChoice::Stop));
        assert_eq!(parse_choice("S"), Some(ExitChoice::Stop));
        assert_eq!(parse_choice("stop"), Some(ExitChoice::Stop));
        assert_eq!(parse_choice("r"), Some(ExitChoice::Remove));
        assert_eq!(parse_choice("Remove"), Some(ExitChoice::Remove));
        assert_eq!(parse_choice("D"), Some(ExitChoice::Detach));
        assert_eq!(parse_choice("x"), None);
        assert_eq!(parse_choice("yes"), None);
    }

    #[test]
    fn choice_file_round_trips_and_is_consumed() {
        let dir = std::env::temp_dir().join(format!("n8-exit-choice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_choice(&dir, "n8-test-otter", ExitChoice::Remove, Some("019ffbd5-53f1-7ab1-8b38-58ead63a8c55"));
        assert_eq!(
            take_choice(&dir, "n8-test-otter"),
            Some((ExitChoice::Remove, Some("019ffbd5-53f1-7ab1-8b38-58ead63a8c55".to_string())))
        );
        // consumed
        assert_eq!(take_choice(&dir, "n8-test-otter"), None);
        // no session id known
        write_choice(&dir, "n8-test-otter", ExitChoice::Stop, None);
        assert_eq!(take_choice(&dir, "n8-test-otter"), Some((ExitChoice::Stop, None)));
        // nothing written (old image) → None, host keeps its legacy behaviour
        assert_eq!(take_choice(&dir, "n8-never-wrote"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_hints() {
        assert_eq!(resume_hint(Some("abc")), "n8 resume abc");
        assert!(resume_hint(None).starts_with("n8 sessions"));
    }
}
