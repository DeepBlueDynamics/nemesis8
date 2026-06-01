//! Content search over session transcripts, powered by the vendored `lume`
//! BM25 primitives.
//!
//! Today's session lookup is a substring match on the UUID + workspace path
//! (see [`crate::session::print_sessions`]). That can't answer "which session
//! was the one where I fixed the antigravity auth" — the answer lives in the
//! transcript body, not the filename. This module builds a field-aware BM25
//! index where each session is one document (title = id/provider/workspace,
//! body = the extracted transcript text) and ranks them against a query.
//!
//! Everything here is local and network-free: we use only `lume::bm25`, never
//! the upstream hybrid/semantic path that calls out to shivvr.nuts.services.
//!
//! NOTE: the index is built on demand per query. For large session corpora a
//! persisted on-disk index (under `~/.nemesis8/`) would be the next step; the
//! BM25 index is plain data and serializes cleanly. Left as a follow-up.

use std::io::{BufRead, BufReader};

use lume::bm25::{Bm25Index, Bm25Params, SearchVariant, Section};

use crate::session::SessionInfo;

/// Cap on extracted text per session, to bound memory and tokenization time
/// on pathologically large transcripts. 1 MiB of text is ~150k tokens —
/// far more than enough for relevance ranking.
const MAX_BODY_BYTES: usize = 1 << 20;

/// Rank `sessions` by BM25 relevance of their transcript content to `query`.
/// Returns `(index_into_sessions, score)` pairs, best score first; sessions
/// with no lexical match are omitted. `index` positions map 1:1 to the input
/// slice (the BM25 builder preserves document order).
pub fn rank_sessions(sessions: &[SessionInfo], query: &str) -> Vec<(usize, f64)> {
    if sessions.is_empty() || query.trim().is_empty() {
        return Vec::new();
    }

    let sections: Vec<Section> = sessions
        .iter()
        .map(|s| Section {
            // Title carries the cheap metadata (id / provider / workspace) so a
            // query for a project name or id fragment still scores, weighted
            // above the body by lume's default title_weight.
            title: format!(
                "{} {} {}",
                s.id,
                s.provider.as_deref().unwrap_or(""),
                s.workspace.as_deref().unwrap_or(""),
            ),
            body: extract_text(&s.path),
            line_number: 0,
            filename: Some(s.path.clone()),
        })
        .collect();

    let index = Bm25Index::build(sections, None);
    index
        .search(query, SearchVariant::Classic, &Bm25Params::default(), None)
        .into_iter()
        .map(|h| (h.section_index, h.score))
        .collect()
}

/// Extract searchable plain text from a session transcript.
///
/// Session files are JSONL (codex / gemini); we parse each line and pull every
/// string leaf — message text, prompts, tool output — which keeps the human's
/// and the agent's words while letting the uniform JSON scaffolding (keys,
/// timestamps, uuids) wash out as high-document-frequency noise. Antigravity
/// `.pb` files are protobuf binaries, so they contribute no body; their
/// id/provider/workspace stay searchable via the section title.
fn extract_text(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.extension().is_some_and(|e| e == "pb") {
        return String::new();
    }
    let Ok(file) = std::fs::File::open(p) else {
        return String::new();
    };

    let mut out = String::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(v) => collect_strings(&v, &mut out),
            // Not JSON (rare/partial line) — index it verbatim.
            Err(_) => {
                out.push_str(&line);
                out.push(' ');
            }
        }
        if out.len() >= MAX_BODY_BYTES {
            break;
        }
    }
    out
}

/// Recursively append every JSON string leaf to `out`, space-separated.
/// Object keys are skipped (they're schema, not content).
fn collect_strings(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, path: &str) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            path: path.to_string(),
            created: None,
            modified: None,
            size_bytes: 0,
            line_count: 0,
            workspace: None,
            provider: None,
        }
    }

    #[test]
    fn ranks_by_transcript_content() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jsonl");
        let b = dir.path().join("b.jsonl");
        std::fs::write(
            &a,
            "{\"role\":\"user\",\"text\":\"fix the antigravity oauth keyring persistence\"}\n",
        )
        .unwrap();
        std::fs::write(
            &b,
            "{\"role\":\"user\",\"text\":\"rename the docker network to gnosis\"}\n",
        )
        .unwrap();

        let sessions = vec![
            session("aaaa", a.to_str().unwrap()),
            session("bbbb", b.to_str().unwrap()),
        ];

        let ranked = rank_sessions(&sessions, "antigravity keyring");
        assert!(!ranked.is_empty(), "expected a content match");
        // Session a (index 0) should rank first for this query.
        assert_eq!(ranked[0].0, 0);
    }

    #[test]
    fn empty_query_returns_nothing() {
        let sessions = vec![session("aaaa", "/nope.jsonl")];
        assert!(rank_sessions(&sessions, "   ").is_empty());
    }

    #[test]
    fn title_metadata_is_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        std::fs::write(&f, "{\"text\":\"hello world\"}\n").unwrap();
        let mut s = session("019c7d80-f629-7452-b38c-ac4ab228d44d", f.to_str().unwrap());
        s.workspace = Some("/home/kord/Code/nemesis8".to_string());
        let ranked = rank_sessions(std::slice::from_ref(&s), "nemesis8");
        assert!(!ranked.is_empty(), "workspace name should match via title");
    }
}
