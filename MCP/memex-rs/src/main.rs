use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;

use anyhow::{Context as AnyhowContext, Result};
use chrono::Utc;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

const MIN_ALPHA: f64 = 0.001;
const MAX_ALPHA: f64 = 0.02;
const DEFAULT_ALPHA: f64 = 0.006;
const EMBED_DIMS: usize = 384;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryRow {
    memory_id: String,
    owner_agent_id: String,
    created_at: i64,
    last_accessed_at: i64,
    fidelity: f64,
    importance: f64,
    decay_alpha: f64,
    access_count: i64,
    consolidation_depth: i64,
    state: String,
    embedding: Vec<f32>,
    graph_centrality: f64,
    keystone: bool,
    role: Option<String>,
    tags: Vec<String>,
    text: String,
    summary: Option<String>,
    source_trace_id: Option<String>,
    artifact_refs: Vec<String>,
    quality_score: f64,
    privacy_scope: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RememberRequest {
    /// Raw event text to store.
    event: String,
    /// Owning agent id.
    owner_agent_id: String,
    /// Optional role/user/system metadata.
    role: Option<String>,
    /// Optional tags.
    tags: Option<Vec<String>>,
    /// Optional quality hint (0..1).
    quality_hint: Option<f64>,
    /// Optional explicit importance (0..1).
    importance: Option<f64>,
    /// Optional privacy scope (default: private).
    privacy_scope: Option<String>,
    /// Optional provenance id.
    source_trace_id: Option<String>,
    /// Optional linked artifacts.
    artifact_refs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RecallRequest {
    /// Query text.
    query: String,
    /// Agent owner to search.
    owner_agent_id: String,
    /// Scope: active | all (default active).
    scope: Option<String>,
    /// Top K (default 8).
    k: Option<usize>,
    /// Recency weight multiplier (default 1.0).
    recency_bias: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OfferRequest {
    /// Existing memory id to share.
    memory_id: String,
    /// Current owner.
    from_agent: String,
    /// New owner.
    to_agent: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DreamRequest {
    /// Owner id for maintenance cycle.
    owner_agent_id: String,
    /// Max memories to evaluate in one pass.
    budget: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StatusRequest {
    /// Owner id.
    owner_agent_id: String,
}

#[derive(Clone)]
struct MemexMcp {
    tool_router: ToolRouter<Self>,
    db: std::sync::Arc<Mutex<Connection>>,
}

#[tool_router]
impl MemexMcp {
    fn new(db_path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(db_path).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db parent dir for {}", db_path))?;
        }
        let conn = Connection::open(db_path).with_context(|| format!("opening sqlite db at {}", db_path))?;
        init_schema(&conn)?;

        Ok(Self {
            tool_router: Self::tool_router(),
            db: std::sync::Arc::new(Mutex::new(conn)),
        })
    }

    #[tool(description = "remember(event, owner_agent_id, role, tags, quality_hint): store a memory record in ACTIVE state")]
    async fn remember(&self, Parameters(req): Parameters<RememberRequest>) -> Result<CallToolResult, ErrorData> {
        if req.event.trim().is_empty() || req.owner_agent_id.trim().is_empty() {
            return Err(ErrorData::invalid_params("event and owner_agent_id are required", None));
        }

        let now = now_ts();
        let memory_id = format!("m_{}", Uuid::new_v4().simple());
        let importance = clamp01(req.importance.or(req.quality_hint).unwrap_or(0.5));
        let quality_score = clamp01(req.quality_hint.unwrap_or(0.9));
        let decay_alpha = (DEFAULT_ALPHA - (importance * 0.002)).clamp(MIN_ALPHA, MAX_ALPHA);
        let tags = req.tags.unwrap_or_default();
        let artifacts = req.artifact_refs.unwrap_or_default();
        let embedding = embed_text(&req.event);

        let embedding_json = serde_json::to_string(&embedding)
            .map_err(|e| ErrorData::internal_error(format!("embedding serialization failed: {e}"), None))?;
        let tags_json = serde_json::to_string(&tags)
            .map_err(|e| ErrorData::internal_error(format!("tags serialization failed: {e}"), None))?;
        let artifacts_json = serde_json::to_string(&artifacts)
            .map_err(|e| ErrorData::internal_error(format!("artifact serialization failed: {e}"), None))?;

        let privacy_scope = req.privacy_scope.unwrap_or_else(|| "private".to_string());

        let mut db = self.db.lock().await;
        db.execute(
            "INSERT INTO memories (
                memory_id, owner_agent_id, created_at, last_accessed_at, fidelity, importance, decay_alpha,
                access_count, consolidation_depth, state, embedding, graph_centrality, keystone,
                role, tags, text, summary, source_trace_id, artifact_refs, quality_score, privacy_scope
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, 'ACTIVE', ?8, 0.0, 0, ?9, ?10, ?11, NULL, ?12, ?13, ?14, ?15)",
            params![
                memory_id,
                req.owner_agent_id,
                now,
                now,
                1.0_f64,
                importance,
                decay_alpha,
                embedding_json,
                req.role,
                tags_json,
                req.event,
                req.source_trace_id,
                artifacts_json,
                quality_score,
                privacy_scope,
            ],
        )
        .map_err(sqlite_err)?;

        append_audit(&db, "remember", &memory_id, &json!({"owner": req.owner_agent_id, "importance": importance})).map_err(sqlite_err)?;

        Ok(json_result(json!({
            "success": true,
            "memory_id": memory_id,
            "state": "ACTIVE",
            "importance": importance,
            "decay_alpha": decay_alpha,
            "quality_score": quality_score
        })))
    }

    #[tool(description = "recall(query, owner_agent_id, scope, k, recency_bias): retrieve ranked memories")]
    async fn recall(&self, Parameters(req): Parameters<RecallRequest>) -> Result<CallToolResult, ErrorData> {
        if req.query.trim().is_empty() || req.owner_agent_id.trim().is_empty() {
            return Err(ErrorData::invalid_params("query and owner_agent_id are required", None));
        }

        let k = req.k.unwrap_or(8).clamp(1, 64);
        let recency_bias = req.recency_bias.unwrap_or(1.0).clamp(0.0, 3.0);
        let scope_all = req.scope.as_deref().unwrap_or("active").eq_ignore_ascii_case("all");

        let mut db = self.db.lock().await;
        let mut rows = load_owner_memories(&db, &req.owner_agent_id, scope_all).map_err(sqlite_err)?;
        let q_vec = embed_text(&req.query);
        let now = now_ts();

        let mut scored: Vec<(f64, MemoryRow)> = rows
            .drain(..)
            .map(|m| {
                let text = if m.text.is_empty() { m.summary.clone().unwrap_or_default() } else { m.text.clone() };
                let lexical = strsim::normalized_levenshtein(&req.query.to_lowercase(), &text.to_lowercase()) as f64;
                let semantic = cosine(&q_vec, &m.embedding) as f64;
                let relevance = 0.45 * lexical + 0.55 * semantic;

                let age_hours = ((now - m.last_accessed_at).max(0) as f64) / 3600.0;
                let recency = (-age_hours / 168.0).exp(); // one-week half-ish life
                let keystone_bonus = if m.keystone { 1.0 } else { 0.0 };

                let score =
                    0.34 * relevance +
                    0.16 * m.fidelity +
                    0.14 * m.importance +
                    0.14 * (recency * recency_bias).min(1.0) +
                    0.12 * m.graph_centrality +
                    0.10 * keystone_bonus;

                (score, m)
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut recalls = Vec::new();
        for (score, row) in scored.into_iter().take(k) {
            let new_access = row.access_count + 1;
            let new_alpha = (row.decay_alpha * 0.97).clamp(MIN_ALPHA, MAX_ALPHA);
            db.execute(
                "UPDATE memories SET access_count=?2, last_accessed_at=?3, decay_alpha=?4 WHERE memory_id=?1",
                params![row.memory_id, new_access, now, new_alpha],
            )
            .map_err(sqlite_err)?;

            recalls.push(json!({
                "memory_id": row.memory_id,
                "state": row.state,
                "score": round4(score),
                "fidelity": round4(row.fidelity),
                "importance": round4(row.importance),
                "graph_centrality": round4(row.graph_centrality),
                "keystone": row.keystone,
                "access_count": new_access,
                "text": truncate_for_context(&row.text, 480),
                "summary": row.summary,
                "tags": row.tags,
                "source_trace_id": row.source_trace_id,
            }));
        }

        Ok(json_result(json!({
            "success": true,
            "query": req.query,
            "owner_agent_id": req.owner_agent_id,
            "count": recalls.len(),
            "recalls": recalls
        })))
    }

    #[tool(description = "offer(memory_id, from_agent, to_agent): share memory between agents by cloning with provenance")]
    async fn offer(&self, Parameters(req): Parameters<OfferRequest>) -> Result<CallToolResult, ErrorData> {
        let mut db = self.db.lock().await;
        let source = load_memory(&db, &req.memory_id, &req.from_agent)
            .map_err(sqlite_err)?
            .ok_or_else(|| ErrorData::invalid_params("memory_id not found for from_agent", None))?;

        let new_id = format!("m_{}", Uuid::new_v4().simple());
        let now = now_ts();

        db.execute(
            "INSERT INTO memories (
                memory_id, owner_agent_id, created_at, last_accessed_at, fidelity, importance, decay_alpha,
                access_count, consolidation_depth, state, embedding, graph_centrality, keystone,
                role, tags, text, summary, source_trace_id, artifact_refs, quality_score, privacy_scope
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                new_id,
                req.to_agent,
                now,
                now,
                source.fidelity,
                source.importance,
                source.decay_alpha,
                source.consolidation_depth + 1,
                source.state,
                serde_json::to_string(&source.embedding).unwrap_or_else(|_| "[]".to_string()),
                source.graph_centrality,
                if source.keystone { 1 } else { 0 },
                source.role,
                serde_json::to_string(&source.tags).unwrap_or_else(|_| "[]".to_string()),
                source.text,
                source.summary,
                Some(req.memory_id.clone()),
                serde_json::to_string(&source.artifact_refs).unwrap_or_else(|_| "[]".to_string()),
                source.quality_score,
                source.privacy_scope,
            ],
        )
        .map_err(sqlite_err)?;

        db.execute(
            "INSERT INTO offers(memory_id, from_agent, to_agent, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![new_id, req.from_agent, req.to_agent, now],
        )
        .map_err(sqlite_err)?;

        append_audit(&db, "offer", &new_id, &json!({"from": req.from_agent, "to": req.to_agent})).map_err(sqlite_err)?;

        Ok(json_result(json!({
            "success": true,
            "shared_memory_id": new_id,
            "from_memory_id": req.memory_id,
            "to_agent": req.to_agent
        })))
    }

    #[tool(description = "dream(owner_agent_id, budget): run decay + promotion + light consolidation")]
    async fn dream(&self, Parameters(req): Parameters<DreamRequest>) -> Result<CallToolResult, ErrorData> {
        let budget = req.budget.unwrap_or(500).clamp(10, 10_000);
        let mut db = self.db.lock().await;
        let now = now_ts();

        let mut memories = load_owner_memories(&db, &req.owner_agent_id, true).map_err(sqlite_err)?;
        if memories.is_empty() {
            return Ok(json_result(json!({"success": true, "owner_agent_id": req.owner_agent_id, "message": "no memories"})));
        }

        memories.truncate(budget);

        // Graph centrality proxy from relative access count.
        let max_access = memories.iter().map(|m| m.access_count).max().unwrap_or(1).max(1) as f64;

        let mut transitioned_forgiven = 0;
        let mut transitioned_archived = 0;
        let mut promoted_keystone = 0;

        for m in memories.iter_mut() {
            let age_days = ((now - m.last_accessed_at).max(0) as f64) / 86_400.0;
            let use_factor = 1.0 / (1.0 + (m.access_count as f64 / 8.0));
            let depth_factor = 1.0 / (1.0 + (m.consolidation_depth as f64 / 4.0));
            let neglect = 1.0 + (age_days / 45.0);
            let effective_alpha = (m.decay_alpha * neglect * use_factor * depth_factor).clamp(MIN_ALPHA, MAX_ALPHA);
            let new_fidelity = (m.fidelity * (-effective_alpha).exp()).clamp(0.0, 1.0);
            let new_centrality = ((m.access_count as f64) / max_access).clamp(0.0, 1.0);

            let mut new_state = m.state.clone();
            let mut new_text = m.text.clone();
            let mut new_summary = m.summary.clone();
            let mut new_keystone = m.keystone;

            if !m.keystone {
                if m.state == "ACTIVE" && new_fidelity < 0.75 && m.quality_score > 0.85 {
                    new_state = "FORGIVEN".to_string();
                    if new_summary.is_none() {
                        new_summary = Some(truncate_for_context(&new_text, 220));
                    }
                    new_text.clear();
                    transitioned_forgiven += 1;
                } else if m.state == "FORGIVEN" && new_fidelity < 0.45 {
                    new_state = "ARCHIVED".to_string();
                    if new_summary.is_none() {
                        new_summary = Some("archived memory".to_string());
                    }
                    new_text.clear();
                    transitioned_archived += 1;
                }
            }

            if !m.keystone && (m.access_count >= 5 || new_centrality >= 0.7) {
                new_keystone = true;
                promoted_keystone += 1;
            }

            db.execute(
                "UPDATE memories
                 SET fidelity=?2, decay_alpha=?3, graph_centrality=?4, state=?5, text=?6, summary=?7, keystone=?8
                 WHERE memory_id=?1",
                params![
                    m.memory_id,
                    new_fidelity,
                    effective_alpha,
                    new_centrality,
                    new_state,
                    new_text,
                    new_summary,
                    if new_keystone { 1 } else { 0 }
                ],
            )
            .map_err(sqlite_err)?;
        }

        // Lightweight consolidation: merge near-duplicates in ACTIVE state.
        let active_now = load_owner_memories(&db, &req.owner_agent_id, false)
            .map_err(sqlite_err)?
            .into_iter()
            .filter(|m| m.state == "ACTIVE")
            .collect::<Vec<_>>();

        let mut consolidated = 0;
        let mut used = HashSet::new();

        for i in 0..active_now.len() {
            if used.contains(&i) {
                continue;
            }
            let mut cluster = vec![i];
            for j in (i + 1)..active_now.len() {
                if used.contains(&j) {
                    continue;
                }
                let sim = cosine(&active_now[i].embedding, &active_now[j].embedding);
                if sim > 0.94 {
                    cluster.push(j);
                }
            }

            if cluster.len() >= 2 {
                for idx in &cluster {
                    used.insert(*idx);
                }

                let ids: Vec<String> = cluster.iter().map(|idx| active_now[*idx].memory_id.clone()).collect();
                let merged_text = cluster
                    .iter()
                    .map(|idx| active_now[*idx].text.clone())
                    .collect::<Vec<_>>()
                    .join("\n---\n");

                let centroid = centroid_embedding(
                    &cluster
                        .iter()
                        .map(|idx| active_now[*idx].embedding.clone())
                        .collect::<Vec<_>>(),
                );

                let new_id = format!("m_{}", Uuid::new_v4().simple());
                let summary = truncate_for_context(&merged_text, 260);
                db.execute(
                    "INSERT INTO memories (
                        memory_id, owner_agent_id, created_at, last_accessed_at, fidelity, importance, decay_alpha,
                        access_count, consolidation_depth, state, embedding, graph_centrality, keystone,
                        role, tags, text, summary, source_trace_id, artifact_refs, quality_score, privacy_scope
                    ) VALUES (?1, ?2, ?3, ?4, 0.98, 0.8, ?5, 0, 1, 'ACTIVE', ?6, 0.6, 1, 'assistant', '[]', '', ?7, ?8, '[]', 0.95, 'private')",
                    params![
                        new_id,
                        req.owner_agent_id,
                        now,
                        now,
                        (DEFAULT_ALPHA * 0.7).clamp(MIN_ALPHA, MAX_ALPHA),
                        serde_json::to_string(&centroid).unwrap_or_else(|_| "[]".to_string()),
                        summary,
                        Some(ids.join(",")),
                    ],
                )
                .map_err(sqlite_err)?;

                for idx in cluster {
                    let id = &active_now[idx].memory_id;
                    db.execute(
                        "UPDATE memories SET state='FORGIVEN', text='', summary=COALESCE(summary, substr(?2,1,240)), consolidation_depth=consolidation_depth+1 WHERE memory_id=?1 AND keystone=0",
                        params![id, active_now[idx].text],
                    )
                    .map_err(sqlite_err)?;
                }

                consolidated += 1;
            }
        }

        append_audit(
            &db,
            "dream",
            &req.owner_agent_id,
            &json!({
                "forgiven": transitioned_forgiven,
                "archived": transitioned_archived,
                "keystone_promoted": promoted_keystone,
                "consolidated_clusters": consolidated
            }),
        )
        .map_err(sqlite_err)?;

        Ok(json_result(json!({
            "success": true,
            "owner_agent_id": req.owner_agent_id,
            "forgiven": transitioned_forgiven,
            "archived": transitioned_archived,
            "keystone_promoted": promoted_keystone,
            "consolidated_clusters": consolidated,
            "budget": budget
        })))
    }

    #[tool(description = "status(owner_agent_id): memory health, counts, and fidelity distribution")]
    async fn status(&self, Parameters(req): Parameters<StatusRequest>) -> Result<CallToolResult, ErrorData> {
        let db = self.db.lock().await;

        let mut stmt = db
            .prepare("SELECT state, COUNT(*) FROM memories WHERE owner_agent_id=?1 GROUP BY state")
            .map_err(sqlite_err)?;
        let mut rows = stmt.query(params![req.owner_agent_id]).map_err(sqlite_err)?;

        let mut by_state: HashMap<String, i64> = HashMap::new();
        while let Some(row) = rows.next().map_err(sqlite_err)? {
            let state: String = row.get(0).map_err(sqlite_err)?;
            let count: i64 = row.get(1).map_err(sqlite_err)?;
            by_state.insert(state, count);
        }

        let total: i64 = by_state.values().sum();

        let avg_fidelity: Option<f64> = db
            .query_row(
                "SELECT AVG(fidelity) FROM memories WHERE owner_agent_id=?1",
                params![req.owner_agent_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(sqlite_err)?
            .flatten();

        let keystone_count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE owner_agent_id=?1 AND keystone=1",
                params![req.owner_agent_id],
                |r| r.get(0),
            )
            .map_err(sqlite_err)?;

        Ok(json_result(json!({
            "success": true,
            "owner_agent_id": req.owner_agent_id,
            "total": total,
            "keystone_count": keystone_count,
            "avg_fidelity": round4(avg_fidelity.unwrap_or(0.0)),
            "by_state": by_state
        })))
    }
}

#[tool_handler]
impl ServerHandler for MemexMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "memex-mcp: thermodynamic keystone memory server. Tools: remember, recall, offer, dream, status. Backed by SQLite; embeddings are deterministic hashed vectors (384d) for local operation."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

fn now_ts() -> i64 {
    Utc::now().timestamp()
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn sqlite_err<E: std::fmt::Display>(e: E) -> ErrorData {
    ErrorData::internal_error(format!("sqlite error: {e}"), None)
}

fn json_result(v: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string()))])
}

fn truncate_for_context(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn embed_text(text: &str) -> Vec<f32> {
    use std::hash::{Hash, Hasher};

    let mut v = vec![0.0_f32; EMBED_DIMS];
    for tok in tokenize(text) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tok.hash(&mut hasher);
        let h = hasher.finish();

        let i1 = (h as usize) % EMBED_DIMS;
        let i2 = ((h >> 16) as usize) % EMBED_DIMS;
        let sign = if (h & 1) == 0 { 1.0 } else { -1.0 };

        v[i1] += 1.0 * sign;
        v[i2] += 0.5 * sign;
    }

    let norm = (v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>()).sqrt() as f32;
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut na = 0.0_f64;
    let mut nb = 0.0_f64;
    for i in 0..a.len() {
        let x = a[i] as f64;
        let y = b[i] as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0)
    }
}

fn centroid_embedding(vecs: &[Vec<f32>]) -> Vec<f32> {
    if vecs.is_empty() {
        return vec![0.0; EMBED_DIMS];
    }
    let mut out = vec![0.0_f32; EMBED_DIMS];
    for v in vecs {
        if v.len() != EMBED_DIMS {
            continue;
        }
        for (i, val) in v.iter().enumerate() {
            out[i] += *val;
        }
    }
    let n = vecs.len().max(1) as f32;
    for x in &mut out {
        *x /= n;
    }
    let norm = (out.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>()).sqrt() as f32;
    if norm > 0.0 {
        for x in &mut out {
            *x /= norm;
        }
    }
    out
}

fn parse_json_vec(value: String) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(&value).unwrap_or_default()
}

fn parse_json_embedding(value: String) -> Vec<f32> {
    serde_json::from_str::<Vec<f32>>(&value).unwrap_or_else(|_| vec![0.0; EMBED_DIMS])
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;

        CREATE TABLE IF NOT EXISTS memories (
            memory_id TEXT PRIMARY KEY,
            owner_agent_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            last_accessed_at INTEGER NOT NULL,
            fidelity REAL NOT NULL,
            importance REAL NOT NULL,
            decay_alpha REAL NOT NULL,
            access_count INTEGER NOT NULL,
            consolidation_depth INTEGER NOT NULL,
            state TEXT NOT NULL,
            embedding TEXT NOT NULL,
            graph_centrality REAL NOT NULL,
            keystone INTEGER NOT NULL,
            role TEXT,
            tags TEXT NOT NULL,
            text TEXT NOT NULL,
            summary TEXT,
            source_trace_id TEXT,
            artifact_refs TEXT NOT NULL,
            quality_score REAL NOT NULL,
            privacy_scope TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_mem_owner_state ON memories(owner_agent_id, state);
        CREATE INDEX IF NOT EXISTS idx_mem_owner_access ON memories(owner_agent_id, access_count DESC);

        CREATE TABLE IF NOT EXISTS offers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            memory_id TEXT NOT NULL,
            from_agent TEXT NOT NULL,
            to_agent TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS audit_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            event TEXT NOT NULL,
            subject_id TEXT NOT NULL,
            payload_json TEXT NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn append_audit(conn: &Connection, event: &str, subject_id: &str, payload: &serde_json::Value) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO audit_log(ts, event, subject_id, payload_json) VALUES (?1, ?2, ?3, ?4)",
        params![now_ts(), event, subject_id, payload.to_string()],
    )?;
    Ok(())
}

fn load_owner_memories(conn: &Connection, owner: &str, include_archived: bool) -> rusqlite::Result<Vec<MemoryRow>> {
    let sql = if include_archived {
        "SELECT memory_id, owner_agent_id, created_at, last_accessed_at, fidelity, importance, decay_alpha,
                access_count, consolidation_depth, state, embedding, graph_centrality, keystone,
                role, tags, text, summary, source_trace_id, artifact_refs, quality_score, privacy_scope
         FROM memories WHERE owner_agent_id=?1"
    } else {
        "SELECT memory_id, owner_agent_id, created_at, last_accessed_at, fidelity, importance, decay_alpha,
                access_count, consolidation_depth, state, embedding, graph_centrality, keystone,
                role, tags, text, summary, source_trace_id, artifact_refs, quality_score, privacy_scope
         FROM memories WHERE owner_agent_id=?1 AND state!='ARCHIVED'"
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params![owner], |r| {
            Ok(MemoryRow {
                memory_id: r.get(0)?,
                owner_agent_id: r.get(1)?,
                created_at: r.get(2)?,
                last_accessed_at: r.get(3)?,
                fidelity: r.get(4)?,
                importance: r.get(5)?,
                decay_alpha: r.get(6)?,
                access_count: r.get(7)?,
                consolidation_depth: r.get(8)?,
                state: r.get(9)?,
                embedding: parse_json_embedding(r.get(10)?),
                graph_centrality: r.get(11)?,
                keystone: {
                    let k: i64 = r.get(12)?;
                    k != 0
                },
                role: r.get(13)?,
                tags: parse_json_vec(r.get(14)?),
                text: r.get(15)?,
                summary: r.get(16)?,
                source_trace_id: r.get(17)?,
                artifact_refs: parse_json_vec(r.get(18)?),
                quality_score: r.get(19)?,
                privacy_scope: r.get(20)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

fn load_memory(conn: &Connection, memory_id: &str, owner: &str) -> rusqlite::Result<Option<MemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT memory_id, owner_agent_id, created_at, last_accessed_at, fidelity, importance, decay_alpha,
                access_count, consolidation_depth, state, embedding, graph_centrality, keystone,
                role, tags, text, summary, source_trace_id, artifact_refs, quality_score, privacy_scope
         FROM memories
         WHERE memory_id=?1 AND owner_agent_id=?2",
    )?;

    stmt.query_row(params![memory_id, owner], |r| {
        Ok(MemoryRow {
            memory_id: r.get(0)?,
            owner_agent_id: r.get(1)?,
            created_at: r.get(2)?,
            last_accessed_at: r.get(3)?,
            fidelity: r.get(4)?,
            importance: r.get(5)?,
            decay_alpha: r.get(6)?,
            access_count: r.get(7)?,
            consolidation_depth: r.get(8)?,
            state: r.get(9)?,
            embedding: parse_json_embedding(r.get(10)?),
            graph_centrality: r.get(11)?,
            keystone: {
                let k: i64 = r.get(12)?;
                k != 0
            },
            role: r.get(13)?,
            tags: parse_json_vec(r.get(14)?),
            text: r.get(15)?,
            summary: r.get(16)?,
            source_trace_id: r.get(17)?,
            artifact_refs: parse_json_vec(r.get(18)?),
            quality_score: r.get(19)?,
            privacy_scope: r.get(20)?,
        })
    })
    .optional()
}

#[tokio::main]
async fn main() -> Result<()> {
    let db_path = env::var("MEMEX_DB_PATH").unwrap_or_else(|_| "./data/memex.sqlite".to_string());
    let server = MemexMcp::new(&db_path)?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
