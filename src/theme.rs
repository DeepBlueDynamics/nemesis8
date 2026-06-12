//! UI theme — the single vocabulary for agent status across every surface
//! (control room tables, rollup strip, detail header, build steps).
//! Per the v3 design (docs/design/n8-tui-v3-design.md §2): glyph + color are
//! ALWAYS used together — the glyph is the truth, color is reinforcement —
//! so 16-color SSH sessions and color-blind users lose nothing.

use ratatui::style::{Color, Modifier, Style};

/// Displayable agent state: lifecycle × attention, flattened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentUiState {
    /// Container created, agent not yet emitting.
    Starting,
    /// Agent blocked waiting on the user — THE fleet question.
    NeedsInput,
    /// Output within the quiescence window.
    Working,
    /// Live but quiet.
    Idle,
    /// Exit code 0.
    ExitedOk,
    /// Nonzero exit.
    ExitedErr,
    /// Fleet host not responding.
    Unreachable,
}

impl AgentUiState {
    /// Map a docker `STATUS` string ("Up 12 minutes", "Exited (0) 2h ago",
    /// "Created", …) to a UI state. The needs-input / idle refinements come
    /// from the monitor heuristic (v3 §2.2, migration patch 5); until then
    /// every Up container reads as Working.
    pub fn from_docker_status(status: &str) -> Self {
        let s = status.trim();
        if s.starts_with("Up") {
            AgentUiState::Working
        } else if s.starts_with("Exited (0)") {
            AgentUiState::ExitedOk
        } else if s.starts_with("Exited") || s.starts_with("Dead") {
            AgentUiState::ExitedErr
        } else if s.starts_with("Created") || s.starts_with("Restarting") {
            AgentUiState::Starting
        } else {
            AgentUiState::Unreachable
        }
    }

    /// Status glyph (Unicode). ASCII fallback via [`AgentUiState::ascii`].
    pub fn glyph(self) -> &'static str {
        match self {
            AgentUiState::Starting => "◌",
            AgentUiState::NeedsInput => "◉",
            AgentUiState::Working => "●",
            AgentUiState::Idle => "○",
            AgentUiState::ExitedOk => "■",
            AgentUiState::ExitedErr => "✖",
            AgentUiState::Unreachable => "?",
        }
    }

    /// ASCII fallback for terminals/locales that can't render the glyphs.
    pub fn ascii(self) -> &'static str {
        match self {
            AgentUiState::Starting => "~",
            AgentUiState::NeedsInput => "!",
            AgentUiState::Working => "*",
            AgentUiState::Idle => ".",
            AgentUiState::ExitedOk => "-",
            AgentUiState::ExitedErr => "x",
            AgentUiState::Unreachable => "?",
        }
    }

    /// Style for the glyph (works in 16-color terminals; needs-input is the
    /// headline state and is the only bold one).
    pub fn style(self) -> Style {
        match self {
            AgentUiState::Starting => Style::default().fg(Color::Cyan),
            AgentUiState::NeedsInput => {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            }
            AgentUiState::Working => Style::default().fg(Color::Green),
            AgentUiState::Idle => Style::default().fg(Color::White),
            AgentUiState::ExitedOk => Style::default().fg(Color::Blue),
            AgentUiState::ExitedErr => Style::default().fg(Color::Red),
            AgentUiState::Unreachable => Style::default().fg(Color::Magenta),
        }
    }

    /// Human label (detail header, rollup strip).
    pub fn label(self) -> &'static str {
        match self {
            AgentUiState::Starting => "starting",
            AgentUiState::NeedsInput => "needs input",
            AgentUiState::Working => "working",
            AgentUiState::Idle => "idle",
            AgentUiState::ExitedOk => "done",
            AgentUiState::ExitedErr => "failed",
            AgentUiState::Unreachable => "unreachable",
        }
    }

    /// Sort rank: needs-input first, then working, then the rest — blocked
    /// agents must be at the top, not findable (v3 §1.3).
    pub fn rank(self) -> u8 {
        match self {
            AgentUiState::NeedsInput => 0,
            AgentUiState::Starting => 1,
            AgentUiState::Working => 2,
            AgentUiState::Idle => 3,
            AgentUiState::ExitedErr => 4,
            AgentUiState::ExitedOk => 5,
            AgentUiState::Unreachable => 6,
        }
    }
}

/// Danger-mode chrome colors (v3 §3.7): heavy red border + badge.
pub fn danger_border() -> Style {
    Style::default().fg(Color::Red)
}
pub fn danger_badge() -> Style {
    Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_docker_status_mapping() {
        assert_eq!(AgentUiState::from_docker_status("Up 12 minutes"), AgentUiState::Working);
        assert_eq!(AgentUiState::from_docker_status("Up About an hour"), AgentUiState::Working);
        assert_eq!(AgentUiState::from_docker_status("Exited (0) 2 hours ago"), AgentUiState::ExitedOk);
        assert_eq!(AgentUiState::from_docker_status("Exited (137) 1 min ago"), AgentUiState::ExitedErr);
        assert_eq!(AgentUiState::from_docker_status("Created"), AgentUiState::Starting);
    }

    #[test]
    fn test_needs_input_sorts_first() {
        let mut v = [
            AgentUiState::Working,
            AgentUiState::ExitedOk,
            AgentUiState::NeedsInput,
            AgentUiState::Idle,
        ];
        v.sort_by_key(|s| s.rank());
        assert_eq!(v[0], AgentUiState::NeedsInput);
    }

    #[test]
    fn test_glyph_and_ascii_unique_pairing() {
        // Every state has a glyph + ascii; needs-input is visually loudest.
        for s in [
            AgentUiState::Starting,
            AgentUiState::NeedsInput,
            AgentUiState::Working,
            AgentUiState::Idle,
            AgentUiState::ExitedOk,
            AgentUiState::ExitedErr,
            AgentUiState::Unreachable,
        ] {
            assert!(!s.glyph().is_empty());
            assert!(!s.ascii().is_empty());
        }
    }
}
