//! Session Analytics — token cost estimation (issue #80).
//!
//! Maps a model id to an approximate ($/Mtok in, $/Mtok out) rate and computes a
//! dollar figure from raw token counts. Pure local arithmetic — no API calls, no
//! network. Rates are coarse public-list defaults and a starting point; they're
//! meant to be overridable from config later (a `[cost.rates]` table). The shape
//! matters more than the exact cents.
//!
//! Independent of the transcript parser: `cost()` takes plain token counts, so
//! it composes with whatever `ToolCall`/`SessionMeta` the parser (#78) yields.

/// ($ per million input tokens, $ per million output tokens) for a model id.
/// Matched most-specific-first on a lowercased id; unknown models fall back to a
/// neutral mid rate so totals stay sane rather than zero.
pub fn rate_for(model: &str) -> (f64, f64) {
    let m = model.to_lowercase();
    // Anthropic
    if m.contains("opus") {
        (15.0, 75.0)
    } else if m.contains("sonnet") {
        (3.0, 15.0)
    } else if m.contains("haiku") {
        (0.80, 4.0)
    }
    // Google
    else if m.contains("gemini") && m.contains("pro") {
        (1.25, 10.0)
    } else if m.contains("gemini") || m.contains("gemma") {
        (0.30, 2.50) // flash-class default
    }
    // OpenAI / codex
    else if m.contains("gpt-4") || m.contains("o4") || m.contains("codex") {
        (2.50, 10.0)
    }
    // Local / open models — effectively free to run, but keep a token of signal.
    else if m.contains("glm") || m.contains("qwen") || m.contains("llama") || m.contains("ollama") {
        (0.0, 0.0)
    }
    // Unknown — neutral fallback so a session still shows a non-zero estimate.
    else {
        (1.0, 5.0)
    }
}

/// Dollar cost of `tokens_in`/`tokens_out` for `model`.
pub fn cost(tokens_in: u64, tokens_out: u64, model: &str) -> f64 {
    let (rin, rout) = rate_for(model);
    (tokens_in as f64 / 1_000_000.0) * rin + (tokens_out as f64 / 1_000_000.0) * rout
}

/// Sum cost across many (tokens_in, tokens_out, model) tuples — for per-session /
/// per-project / per-org rollups.
pub fn cost_total<'a, I>(rows: I) -> f64
where
    I: IntoIterator<Item = (u64, u64, &'a str)>,
{
    rows.into_iter().map(|(i, o, m)| cost(i, o, m)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_match_by_family() {
        assert_eq!(rate_for("claude-opus-4-8"), (15.0, 75.0));
        assert_eq!(rate_for("claude-sonnet-4-6"), (3.0, 15.0));
        assert_eq!(rate_for("claude-haiku-4-5-20251001"), (0.80, 4.0));
        assert_eq!(rate_for("glm-5.2"), (0.0, 0.0));
    }

    #[test]
    fn unknown_model_uses_neutral_fallback() {
        assert_eq!(rate_for("some-future-model"), (1.0, 5.0));
    }

    #[test]
    fn cost_math() {
        // 1M in @ $3 + 0.5M out @ $15 = 3 + 7.5 = 10.5
        let c = cost(1_000_000, 500_000, "claude-sonnet-4-6");
        assert!((c - 10.5).abs() < 1e-9);
        // local model → free
        assert_eq!(cost(2_000_000, 1_000_000, "glm-5.2"), 0.0);
    }

    #[test]
    fn rollup_sums() {
        let rows = vec![
            (1_000_000u64, 0u64, "claude-haiku-4-5"),
            (0, 1_000_000, "claude-sonnet-4-6"),
        ];
        // 0.80 + 15.0 = 15.8
        assert!((cost_total(rows) - 15.8).abs() < 1e-9);
    }
}
