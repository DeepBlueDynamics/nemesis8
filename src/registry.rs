//! Agent registry — the control plane's record of which agents exist, their
//! state, and which container backs them.
//!
//! Mirrors `scheduler::TriggerStore`: an in-memory `Vec` persisted to JSON
//! under ~/.nemesis8/home/agents.json. The key behavior beyond simple
//! tracking is `reconcile()`, which folds the live `docker ps` view into the
//! registry — so agents started outside the API (interactive `n8 run`, `agy`
//! sessions, anything launched by hand) show up too, and agents whose
//! container has vanished get marked Exited.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Running,
    Idle,
    Exited,
    Killed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSource {
    /// Spawned through the control-plane API.
    Spawned,
    /// Adopted from `docker ps` during reconciliation (started outside the API).
    Discovered,
    /// Self-registered by the container's entry binary on boot.
    Registered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Global id: `{host_id}/{local_id}` so control routes to the owning daemon.
    pub id: String,
    pub host_id: String,
    pub local_id: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub container_id: Option<String>,
    #[serde(default)]
    pub container_name: Option<String>,
    pub state: AgentState,
    pub source: AgentSource,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_prompt: Option<String>,
}

impl AgentRecord {
    pub fn global_id(host_id: &str, local_id: &str) -> String {
        format!("{host_id}/{local_id}")
    }
}

/// A worker daemon known to the controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRecord {
    pub host_id: String,
    /// Base URL the controller uses to reach this daemon (e.g. http://server:40008).
    pub url: String,
    pub role: String,
    pub last_seen: DateTime<Utc>,
    #[serde(default)]
    pub agent_count: usize,
}

/// Persistent agent registry (JSON file).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    pub agents: Vec<AgentRecord>,
    /// Worker daemons that have registered up (controller side).
    #[serde(default)]
    pub daemons: Vec<DaemonRecord>,
}

impl Registry {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading registry from {}", path.display()))?;
        serde_json::from_str(&content).with_context(|| "parsing registry JSON")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)
            .with_context(|| format!("writing registry to {}", path.display()))
    }

    pub fn get(&self, id: &str) -> Option<&AgentRecord> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub fn upsert(&mut self, record: AgentRecord) {
        if let Some(existing) = self.agents.iter_mut().find(|a| a.id == record.id) {
            *existing = record;
        } else {
            self.agents.push(record);
        }
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.agents.len();
        self.agents.retain(|a| a.id != id);
        self.agents.len() < before
    }

    pub fn mark_state(&mut self, id: &str, state: AgentState) -> bool {
        if let Some(a) = self.agents.iter_mut().find(|a| a.id == id) {
            a.state = state;
            a.last_seen = Some(Utc::now());
            true
        } else {
            false
        }
    }

    // ── Fleet (controller side) ──

    /// Upsert a worker daemon registration.
    pub fn upsert_daemon(&mut self, rec: DaemonRecord) {
        if let Some(existing) = self.daemons.iter_mut().find(|d| d.host_id == rec.host_id) {
            *existing = rec;
        } else {
            self.daemons.push(rec);
        }
    }

    /// Find the daemon that owns a given host_id.
    pub fn daemon_for_host(&self, host_id: &str) -> Option<&DaemonRecord> {
        self.daemons.iter().find(|d| d.host_id == host_id)
    }

    /// Replace ALL agents belonging to a host with the supplied snapshot.
    /// Used by /agents/sync so a worker's pushed view is authoritative for
    /// its own host (exited agents simply drop out of subsequent snapshots).
    pub fn replace_host_agents(&mut self, host_id: &str, mut agents: Vec<AgentRecord>) {
        self.agents.retain(|a| a.host_id != host_id);
        self.agents.append(&mut agents);
        if let Some(d) = self.daemons.iter_mut().find(|d| d.host_id == host_id) {
            d.agent_count = self.agents.iter().filter(|a| a.host_id == host_id).count();
            d.last_seen = Utc::now();
        }
    }

    /// Fold the live container list (this host) into the registry.
    /// - labeled/known containers present → state Running, refresh container id/name
    /// - present container with no matching record → adopt as Discovered/Running
    /// - record for this host whose container is gone → mark Exited
    ///
    /// `containers` is the filtered nemesis8 container list from
    /// `DockerOps::list_containers`.
    pub fn reconcile(
        &mut self,
        containers: &[bollard::models::ContainerSummary],
        host_id: &str,
    ) {
        use crate::docker::{LABEL_AGENT_ID, LABEL_PROVIDER};

        let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();

        for c in containers {
            let labels = c.labels.clone().unwrap_or_default();
            let cname = c
                .names
                .as_ref()
                .and_then(|n| n.first())
                .map(|n| n.trim_start_matches('/').to_string());
            // local_id preference: agent_id label > container name > short id
            let local_id = labels
                .get(LABEL_AGENT_ID)
                .cloned()
                .or_else(|| cname.clone())
                .or_else(|| c.id.as_ref().map(|i| i.chars().take(12).collect()))
                .unwrap_or_else(|| "unknown".to_string());
            let gid = AgentRecord::global_id(host_id, &local_id);
            present.insert(gid.clone());

            let provider = labels.get(LABEL_PROVIDER).cloned();
            let now = Utc::now();

            if let Some(rec) = self.agents.iter_mut().find(|a| a.id == gid) {
                rec.container_id = c.id.clone();
                rec.container_name = cname;
                rec.last_seen = Some(now);
                if rec.state == AgentState::Exited || rec.state == AgentState::Killed {
                    rec.state = AgentState::Running; // reappeared
                }
                if rec.provider.is_none() {
                    rec.provider = provider;
                }
            } else {
                self.agents.push(AgentRecord {
                    id: gid,
                    host_id: host_id.to_string(),
                    local_id,
                    provider,
                    workspace: None,
                    container_id: c.id.clone(),
                    container_name: cname,
                    state: AgentState::Running,
                    source: AgentSource::Discovered,
                    started_at: Some(now),
                    last_seen: Some(now),
                    last_prompt: None,
                });
            }
        }

        // Anything on this host that we thought was live but isn't present → Exited.
        for rec in self.agents.iter_mut() {
            if rec.host_id == host_id
                && !present.contains(&rec.id)
                && matches!(
                    rec.state,
                    AgentState::Running | AgentState::Starting | AgentState::Idle
                )
            {
                rec.state = AgentState::Exited;
                rec.last_seen = Some(Utc::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_and_get() {
        let mut r = Registry::default();
        r.upsert(AgentRecord {
            id: "h/a".into(),
            host_id: "h".into(),
            local_id: "a".into(),
            provider: Some("codex".into()),
            workspace: None,
            container_id: None,
            container_name: None,
            state: AgentState::Running,
            source: AgentSource::Spawned,
            started_at: None,
            last_seen: None,
            last_prompt: None,
        });
        assert!(r.get("h/a").is_some());
        assert!(r.mark_state("h/a", AgentState::Killed));
        assert_eq!(r.get("h/a").unwrap().state, AgentState::Killed);
        assert!(r.remove("h/a"));
        assert!(r.get("h/a").is_none());
    }
}
