use anyhow::{Context, Result};
use chrono::{DateTime, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Schedule mode for a trigger
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    /// Fire at a specific ISO timestamp, then never again
    Once { at: DateTime<Utc> },
    /// Fire daily at HH:MM in the given IANA timezone (`America/Los_Angeles`,
    /// `Europe/Berlin`, `UTC`). Unknown zones are rejected at create time and
    /// treated as UTC if one slips into the store.
    Daily {
        time: String,
        #[serde(default = "default_tz")]
        timezone: String,
    },
    /// Fire every N minutes
    Interval { minutes: u64 },
}

fn default_tz() -> String {
    "UTC".to_string()
}

/// Is `name` a timezone the scheduler can resolve? IANA names and `UTC`.
pub fn valid_timezone(name: &str) -> bool {
    name.parse::<chrono_tz::Tz>().is_ok()
}

/// The UTC instant of a wall-clock time in `tz`. A time inside a DST gap (it
/// never happens on that day) rolls forward one hour; a time inside a DST
/// overlap (it happens twice) takes the earlier instant.
fn resolve_wall_time(tz: &chrono_tz::Tz, wall: chrono::NaiveDateTime) -> Option<DateTime<Utc>> {
    let pick = |r: chrono::LocalResult<DateTime<chrono_tz::Tz>>| match r {
        chrono::LocalResult::Single(dt) => Some(dt),
        chrono::LocalResult::Ambiguous(first, _) => Some(first),
        chrono::LocalResult::None => None,
    };
    pick(tz.from_local_datetime(&wall))
        .or_else(|| pick(tz.from_local_datetime(&(wall + chrono::Duration::hours(1)))))
        .map(|dt| dt.with_timezone(&Utc))
}

/// Timeout for a scheduled or spawned agent run when the request names none.
/// Agent runs routinely take minutes (a sticky run reads, edits, reports), so
/// this is far above the gateway's synchronous `/completion` default.
pub const DEFAULT_RUN_TIMEOUT_SECS: u64 = 900;
/// Lower/upper bounds accepted for a per-run `timeout_secs`.
pub const MIN_RUN_TIMEOUT_SECS: u64 = 10;
pub const MAX_RUN_TIMEOUT_SECS: u64 = 86_400;

/// Per-trigger run overrides — where and how a scheduled fire executes. When
/// set, the scheduler loads this `workspace`'s layered config (like resume does,
/// so the workspace's `mcp_tools`/env drive the container) and runs with this
/// `provider`/`model`/`danger`, instead of the gateway's defaults. All-None =
/// gateway defaults (back-compat with triggers created before this existed).
///
/// `env` / `labels` / `identity` / `timeout_secs` reach the run's container:
/// extra environment (n8's own variables win on a clash), extra Docker labels
/// (`nemesis8.*` is reserved), a requested agent name (container name, agent id
/// and Hyperia identity `nemesis8/<identity>` all at once), and how long the
/// run may take before it is stopped.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RunOpts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danger: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl RunOpts {
    /// The effective timeout for a run under these options.
    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs.unwrap_or(DEFAULT_RUN_TIMEOUT_SECS)
    }
}

/// A scheduled trigger record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRecord {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub schedule: Schedule,
    pub prompt_text: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// When the last run was LAUNCHED. `last_status` is "running" until it ends.
    #[serde(default)]
    pub last_fired: Option<DateTime<Utc>>,
    /// "running" | "ok" | "error" (or unset before the first fire).
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// When the last run ended (either way). Unset while it runs.
    #[serde(default)]
    pub last_finished_at: Option<DateTime<Utc>>,
    /// Agent id (== container name) of the last run, set the moment the
    /// container is named — so `GET /agents/{last_agent_id}` works while it
    /// runs. Unset if the run failed before a container existed.
    #[serde(default)]
    pub last_agent_id: Option<String>,
    /// Provider session id of the last run, reported by the container's entry
    /// once the provider writes its session. Unset if it never appeared.
    #[serde(default)]
    pub last_session_id: Option<String>,
    /// Where/how this trigger runs when fired. Default = gateway's config.
    #[serde(default)]
    pub run: RunOpts,
}

fn default_enabled() -> bool {
    true
}

impl TriggerRecord {
    /// Compute the next fire time from now
    pub fn next_fire(&self) -> Option<DateTime<Utc>> {
        self.next_fire_from(Utc::now())
    }

    /// Compute the next fire time as seen from `now` (the clock is injected so
    /// the daily/timezone arithmetic is testable).
    pub fn next_fire_from(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if !self.enabled {
            return None;
        }

        match &self.schedule {
            Schedule::Once { at } => {
                if self.last_fired.is_some() {
                    None // Already fired
                } else if *at > now {
                    Some(*at)
                } else {
                    Some(now) // Overdue, fire immediately
                }
            }
            Schedule::Daily { time, timezone } => {
                let target = NaiveTime::parse_from_str(time, "%H:%M").ok()?;
                // HH:MM is wall-clock time IN `timezone`. Resolve yesterday's,
                // today's and tomorrow's instants there: the latest one at or
                // before `now` is the most recent due time, the earliest one
                // after it is the next. A due time nobody has fired for yet
                // (later than the last fire, or than creation for a trigger
                // that never fired) is overdue and fires now — that is what
                // makes a daily trigger fire at all, since the scheduler only
                // fires what `next_fire <= now`. A trigger created after
                // today's time has passed waits for tomorrow instead of firing
                // on creation.
                let tz: chrono_tz::Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
                let today = now.with_timezone(&tz).date_naive();
                let days = [today.pred_opt()?, today, today.succ_opt()?];
                let instants = days
                    .iter()
                    .filter_map(|day| resolve_wall_time(&tz, day.and_time(target)));
                let mut prev_due: Option<DateTime<Utc>> = None;
                let mut next: Option<DateTime<Utc>> = None;
                for instant in instants {
                    if instant <= now {
                        prev_due = Some(instant);
                    } else if next.is_none() {
                        next = Some(instant);
                    }
                }
                let anchor = self.last_fired.or(self.created_at);
                match (prev_due, anchor) {
                    (Some(due), Some(a)) if a < due => Some(now), // overdue
                    _ => next,
                }
            }
            Schedule::Interval { minutes } => {
                let interval = chrono::Duration::minutes(*minutes as i64);
                match self.last_fired {
                    Some(last) => {
                        let next = last + interval;
                        if next > now {
                            Some(next)
                        } else {
                            Some(now) // Overdue
                        }
                    }
                    None => Some(now), // Never fired, fire now
                }
            }
        }
    }

    /// Check if this trigger should fire now
    pub fn should_fire(&self) -> bool {
        self.next_fire()
            .is_some_and(|next| next <= Utc::now())
    }
}

/// Persistent trigger store (JSON file)
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TriggerStore {
    pub triggers: Vec<TriggerRecord>,
}

impl TriggerStore {
    /// Load triggers from a JSON file
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading triggers from {}", path.display()))?;
        let store: Self = serde_json::from_str(&content)
            .with_context(|| "parsing trigger store JSON")?;
        Ok(store)
    }

    /// Save triggers to a JSON file
    pub fn save(&self, path: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)
            .with_context(|| format!("writing triggers to {}", path.display()))?;
        Ok(())
    }

    /// Add or update a trigger
    pub fn upsert(&mut self, trigger: TriggerRecord) {
        if let Some(existing) = self.triggers.iter_mut().find(|t| t.id == trigger.id) {
            *existing = trigger;
        } else {
            self.triggers.push(trigger);
        }
    }

    /// Remove a trigger by ID
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.triggers.len();
        self.triggers.retain(|t| t.id != id);
        self.triggers.len() < before
    }

    /// Get all triggers that should fire now
    pub fn due_triggers(&self) -> Vec<&TriggerRecord> {
        self.triggers.iter().filter(|t| t.should_fire()).collect()
    }

    /// Mark a trigger as fired
    pub fn mark_fired(&mut self, id: &str) {
        if let Some(trigger) = self.triggers.iter_mut().find(|t| t.id == id) {
            trigger.last_fired = Some(Utc::now());

            // Disable one-shot triggers after firing
            if matches!(trigger.schedule, Schedule::Once { .. }) {
                trigger.enabled = false;
            }
        }
    }

    /// Reload the store from `path`, apply `f` to the trigger `id`, and save.
    /// `Ok(false)` when the trigger no longer exists (deleted while a run was
    /// in flight): nothing is written, so a finished run never resurrects a
    /// trigger the user removed. Callers serialize this behind a lock.
    pub fn patch(path: &Path, id: &str, f: impl FnOnce(&mut TriggerRecord)) -> Result<bool> {
        let mut store = Self::load(path)?;
        let Some(trigger) = store.triggers.iter_mut().find(|t| t.id == id) else {
            return Ok(false);
        };
        f(trigger);
        store.save(path)?;
        Ok(true)
    }
}

/// Simple template renderer: replaces {{key}} with values
pub fn render_template(template: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{{{key}}}}}"), value);
    }
    result
}

/// Run the scheduler loop (used inside gateway serve mode)
pub async fn scheduler_loop(store_path: std::path::PathBuf, interval_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;

        let mut store = match TriggerStore::load(&store_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to load triggers: {e}");
                continue;
            }
        };

        let due: Vec<String> = store
            .due_triggers()
            .iter()
            .map(|t| t.id.clone())
            .collect();

        for id in &due {
            if let Some(trigger) = store.triggers.iter().find(|t| &t.id == id) {
                tracing::info!(
                    trigger_id = %id,
                    title = %trigger.title,
                    "firing scheduled trigger"
                );

                // TODO: dispatch the trigger prompt to codex
                // For now, just log and mark as fired
            }
            store.mark_fired(id);
        }

        if !due.is_empty() {
            if let Err(e) = store.save(&store_path) {
                tracing::warn!("failed to save trigger state: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interval_trigger_should_fire() {
        let trigger = TriggerRecord {
            id: "test-1".to_string(),
            title: "Test".to_string(),
            description: String::new(),
            schedule: Schedule::Interval { minutes: 5 },
            prompt_text: "hello".to_string(),
            created_by: String::new(),
            created_at: Some(Utc::now()),
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        };
        assert!(trigger.should_fire()); // Never fired, should fire immediately
    }

    #[test]
    fn test_once_trigger_after_fire() {
        let trigger = TriggerRecord {
            id: "test-2".to_string(),
            title: "One-shot".to_string(),
            description: String::new(),
            schedule: Schedule::Once {
                at: Utc::now() - chrono::Duration::hours(1),
            },
            prompt_text: "hello".to_string(),
            created_by: String::new(),
            created_at: Some(Utc::now()),
            enabled: true,
            tags: vec![],
            last_fired: Some(Utc::now() - chrono::Duration::minutes(30)),
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        };
        assert!(!trigger.should_fire()); // Already fired
    }

    #[test]
    fn test_render_template() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("name".to_string(), "world".to_string());
        let result = render_template("hello {{name}}", &vars);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_render_template_missing_key() {
        let vars = std::collections::HashMap::new();
        let result = render_template("hello {{name}}", &vars);
        // Missing keys are left as-is
        assert_eq!(result, "hello {{name}}");
    }

    #[test]
    fn test_render_template_multiple_vars() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("greeting".to_string(), "hi".to_string());
        vars.insert("name".to_string(), "kord".to_string());
        let result = render_template("{{greeting}} {{name}}!", &vars);
        assert_eq!(result, "hi kord!");
    }

    #[test]
    fn test_once_trigger_future_should_not_fire() {
        let trigger = TriggerRecord {
            id: "future".to_string(),
            title: "Future event".to_string(),
            description: String::new(),
            schedule: Schedule::Once {
                at: Utc::now() + chrono::Duration::hours(24),
            },
            prompt_text: "check later".to_string(),
            created_by: String::new(),
            created_at: Some(Utc::now()),
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        };
        assert!(!trigger.should_fire());
        assert!(trigger.next_fire().is_some());
    }

    #[test]
    fn test_disabled_trigger_never_fires() {
        let trigger = TriggerRecord {
            id: "disabled".to_string(),
            title: "Disabled".to_string(),
            description: String::new(),
            schedule: Schedule::Interval { minutes: 1 },
            prompt_text: "nope".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: false,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        };
        assert!(!trigger.should_fire());
        assert!(trigger.next_fire().is_none());
    }

    #[test]
    fn test_daily_trigger_has_next_fire() {
        let trigger = TriggerRecord {
            id: "daily".to_string(),
            title: "Daily check".to_string(),
            description: String::new(),
            schedule: Schedule::Daily {
                time: "03:00".to_string(),
                timezone: "UTC".to_string(),
            },
            prompt_text: "daily prompt".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        };
        let next = trigger.next_fire();
        assert!(next.is_some());
        // next_fire should be in the future or today
        let fire_time = next.unwrap();
        assert!(fire_time >= Utc::now() - chrono::Duration::seconds(1));
    }

    #[test]
    fn test_interval_not_yet_due() {
        let trigger = TriggerRecord {
            id: "interval".to_string(),
            title: "Recent".to_string(),
            description: String::new(),
            schedule: Schedule::Interval { minutes: 60 },
            prompt_text: "check".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: Some(Utc::now()), // Just fired
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        };
        assert!(!trigger.should_fire());
    }

    #[test]
    fn test_trigger_store_upsert_new() {
        let mut store = TriggerStore::default();
        assert_eq!(store.triggers.len(), 0);

        store.upsert(TriggerRecord {
            id: "t1".to_string(),
            title: "First".to_string(),
            description: String::new(),
            schedule: Schedule::Interval { minutes: 5 },
            prompt_text: "hello".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        });

        assert_eq!(store.triggers.len(), 1);
        assert_eq!(store.triggers[0].title, "First");
    }

    #[test]
    fn test_trigger_store_upsert_update() {
        let mut store = TriggerStore::default();
        store.upsert(TriggerRecord {
            id: "t1".to_string(),
            title: "Original".to_string(),
            description: String::new(),
            schedule: Schedule::Interval { minutes: 5 },
            prompt_text: "hello".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        });

        // Upsert same ID with different title
        store.upsert(TriggerRecord {
            id: "t1".to_string(),
            title: "Updated".to_string(),
            description: String::new(),
            schedule: Schedule::Interval { minutes: 10 },
            prompt_text: "world".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        });

        assert_eq!(store.triggers.len(), 1);
        assert_eq!(store.triggers[0].title, "Updated");
    }

    #[test]
    fn test_trigger_store_remove() {
        let mut store = TriggerStore::default();
        store.upsert(TriggerRecord {
            id: "t1".to_string(),
            title: "To Remove".to_string(),
            description: String::new(),
            schedule: Schedule::Interval { minutes: 5 },
            prompt_text: "bye".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        });

        assert!(store.remove("t1"));
        assert_eq!(store.triggers.len(), 0);
    }

    #[test]
    fn test_trigger_store_remove_nonexistent() {
        let mut store = TriggerStore::default();
        assert!(!store.remove("nope"));
    }

    #[test]
    fn test_trigger_store_mark_fired_disables_once() {
        let mut store = TriggerStore::default();
        store.upsert(TriggerRecord {
            id: "once".to_string(),
            title: "One-shot".to_string(),
            description: String::new(),
            schedule: Schedule::Once {
                at: Utc::now() - chrono::Duration::hours(1),
            },
            prompt_text: "fire once".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        });

        store.mark_fired("once");
        assert!(!store.triggers[0].enabled);
        assert!(store.triggers[0].last_fired.is_some());
    }

    #[test]
    fn test_trigger_store_mark_fired_keeps_interval_enabled() {
        let mut store = TriggerStore::default();
        store.upsert(TriggerRecord {
            id: "int".to_string(),
            title: "Interval".to_string(),
            description: String::new(),
            schedule: Schedule::Interval { minutes: 5 },
            prompt_text: "repeat".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        });

        store.mark_fired("int");
        assert!(store.triggers[0].enabled); // Interval stays enabled
        assert!(store.triggers[0].last_fired.is_some());
    }

    #[test]
    fn test_trigger_store_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("triggers.json");

        let mut store = TriggerStore::default();
        store.upsert(TriggerRecord {
            id: "persist".to_string(),
            title: "Persistent".to_string(),
            description: "survives disk".to_string(),
            schedule: Schedule::Interval { minutes: 10 },
            prompt_text: "check disk".to_string(),
            created_by: "test".to_string(),
            created_at: Some(Utc::now()),
            enabled: true,
            tags: vec!["test".to_string()],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        });
        store.save(&path).unwrap();

        // Load back
        let loaded = TriggerStore::load(&path).unwrap();
        assert_eq!(loaded.triggers.len(), 1);
        assert_eq!(loaded.triggers[0].id, "persist");
        assert_eq!(loaded.triggers[0].title, "Persistent");
        assert_eq!(loaded.triggers[0].tags, vec!["test"]);
    }

    #[test]
    fn test_trigger_store_load_missing_file() {
        let store = TriggerStore::load(std::path::Path::new("/nonexistent.json")).unwrap();
        assert!(store.triggers.is_empty());
    }

    #[test]
    fn test_schedule_json_roundtrip() {
        let trigger = TriggerRecord {
            id: "rt".to_string(),
            title: "Roundtrip".to_string(),
            description: String::new(),
            schedule: Schedule::Daily {
                time: "14:30".to_string(),
                timezone: "US/Eastern".to_string(),
            },
            prompt_text: "afternoon check".to_string(),
            created_by: String::new(),
            created_at: None,
            enabled: true,
            tags: vec![],
            last_fired: None,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        };

        let json = serde_json::to_string(&trigger).unwrap();
        let parsed: TriggerRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "rt");
        match parsed.schedule {
            Schedule::Daily { time, timezone } => {
                assert_eq!(time, "14:30");
                assert_eq!(timezone, "US/Eastern");
            }
            _ => panic!("expected Daily schedule"),
        }
    }
}

#[cfg(test)]
mod run_option_tests {
    use super::*;
    use chrono::TimeZone;

    fn daily(
        time: &str,
        tz: &str,
        created_at: DateTime<Utc>,
        last_fired: Option<DateTime<Utc>>,
    ) -> TriggerRecord {
        TriggerRecord {
            id: "d".into(),
            title: "daily".into(),
            description: String::new(),
            schedule: Schedule::Daily {
                time: time.into(),
                timezone: tz.into(),
            },
            prompt_text: "p".into(),
            created_by: String::new(),
            created_at: Some(created_at),
            enabled: true,
            tags: vec![],
            last_fired,
            last_status: None,
            last_error: None,
            last_finished_at: None,
            last_agent_id: None,
            last_session_id: None,
            run: RunOpts::default(),
        }
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    #[test]
    fn daily_time_is_wall_clock_in_the_named_timezone() {
        // 09:00 in Los Angeles is 17:00 UTC in January (PST) ...
        let now = utc(2026, 1, 15, 0, 0, 0);
        let t = daily("09:00", "America/Los_Angeles", now, None);
        assert_eq!(t.next_fire_from(now), Some(utc(2026, 1, 15, 17, 0, 0)));
        // ... and 16:00 UTC in July (PDT).
        let now = utc(2026, 7, 15, 0, 0, 0);
        let t = daily("09:00", "America/Los_Angeles", now, None);
        assert_eq!(t.next_fire_from(now), Some(utc(2026, 7, 15, 16, 0, 0)));
        // Berlin, ahead of UTC: 09:00 CET is 08:00 UTC.
        let now = utc(2026, 1, 15, 0, 0, 0);
        let t = daily("09:00", "Europe/Berlin", now, None);
        assert_eq!(t.next_fire_from(now), Some(utc(2026, 1, 15, 8, 0, 0)));
    }

    #[test]
    fn daily_fires_once_when_its_time_passes_then_waits_a_day() {
        let created = utc(2026, 1, 15, 10, 0, 0);
        let t = daily("12:00", "UTC", created, None);
        // Before noon: next is today's noon, not due.
        let before = utc(2026, 1, 15, 11, 59, 0);
        assert_eq!(t.next_fire_from(before), Some(utc(2026, 1, 15, 12, 0, 0)));
        // A tick after noon: overdue, fires now.
        let after = utc(2026, 1, 15, 12, 0, 30);
        assert_eq!(t.next_fire_from(after), Some(after));
        // Fired: the next one is tomorrow's noon, not "now" again.
        let fired = daily("12:00", "UTC", created, Some(after));
        let later = utc(2026, 1, 15, 12, 1, 0);
        assert_eq!(fired.next_fire_from(later), Some(utc(2026, 1, 16, 12, 0, 0)));
        // Still overdue a day later if the gateway was down: fires once on wake.
        let wake = utc(2026, 1, 17, 3, 0, 0);
        assert_eq!(fired.next_fire_from(wake), Some(wake));
    }

    #[test]
    fn daily_created_after_todays_time_waits_for_tomorrow() {
        let created = utc(2026, 1, 15, 14, 0, 0);
        let t = daily("12:00", "UTC", created, None);
        let now = utc(2026, 1, 15, 14, 0, 10);
        assert_eq!(t.next_fire_from(now), Some(utc(2026, 1, 16, 12, 0, 0)));
    }

    #[test]
    fn unknown_timezone_reads_as_utc_and_is_reported_invalid() {
        assert!(valid_timezone("UTC"));
        assert!(valid_timezone("Europe/Berlin"));
        assert!(!valid_timezone("Mars/Olympus"));
        let now = utc(2026, 1, 15, 0, 0, 0);
        let t = daily("12:00", "Mars/Olympus", now, None);
        assert_eq!(t.next_fire_from(now), Some(utc(2026, 1, 15, 12, 0, 0)));
    }

    #[test]
    fn dst_gap_rolls_forward_an_hour() {
        // 2026-03-08 02:30 does not exist in New York (clocks jump from 02:00
        // to 03:00); it resolves to 03:30 EDT = 07:30 UTC.
        let now = utc(2026, 3, 8, 0, 0, 0);
        let t = daily("02:30", "America/New_York", now, None);
        assert_eq!(t.next_fire_from(now), Some(utc(2026, 3, 8, 7, 30, 0)));
    }

    #[test]
    fn legacy_trigger_json_without_run_extras_still_parses() {
        let json = r#"{"id":"a","title":"t","schedule":{"type":"interval","minutes":5},
            "prompt_text":"p","run":{"workspace":"/w"}}"#;
        let t: TriggerRecord = serde_json::from_str(json).unwrap();
        assert_eq!(t.run.workspace.as_deref(), Some("/w"));
        assert!(t.run.env.is_empty() && t.run.labels.is_empty());
        assert_eq!(t.run.identity, None);
        assert_eq!(t.run.timeout_secs(), DEFAULT_RUN_TIMEOUT_SECS);
        assert_eq!(t.last_agent_id, None);
        assert_eq!(t.last_session_id, None);
        assert_eq!(t.last_finished_at, None);
    }

    #[test]
    fn run_opts_roundtrip_and_omit_empty_extras() {
        let plain = serde_json::to_value(RunOpts::default()).unwrap();
        assert_eq!(plain, serde_json::json!({}));
        let full = RunOpts {
            env: BTreeMap::from([("HYPERIA_STICKY_ID".to_string(), "42".to_string())]),
            labels: BTreeMap::from([("hyperia.sticky".to_string(), "42".to_string())]),
            identity: Some("sticky-42".into()),
            timeout_secs: Some(600),
            ..Default::default()
        };
        let json = serde_json::to_value(&full).unwrap();
        assert_eq!(json["env"]["HYPERIA_STICKY_ID"], "42");
        assert_eq!(json["timeout_secs"], 600);
        let back: RunOpts = serde_json::from_value(json).unwrap();
        assert_eq!(back, full);
        assert_eq!(back.timeout_secs(), 600);
    }

    #[test]
    fn patch_updates_one_trigger_and_leaves_a_deleted_one_deleted() {
        let path = std::env::temp_dir().join(format!("n8-patch-{}.json", uuid::Uuid::new_v4()));
        let mut store = TriggerStore::default();
        store.upsert(daily("12:00", "UTC", utc(2026, 1, 1, 0, 0, 0), None));
        store.save(&path).unwrap();

        assert!(TriggerStore::patch(&path, "d", |t| t.last_agent_id = Some("n8-fun-lark".into())).unwrap());
        let reloaded = TriggerStore::load(&path).unwrap();
        assert_eq!(reloaded.triggers[0].last_agent_id.as_deref(), Some("n8-fun-lark"));

        // Deleted meanwhile: the patch is a no-op and writes nothing back.
        let mut store = TriggerStore::load(&path).unwrap();
        assert!(store.remove("d"));
        store.save(&path).unwrap();
        assert!(!TriggerStore::patch(&path, "d", |t| t.last_status = Some("ok".into())).unwrap());
        assert!(TriggerStore::load(&path).unwrap().triggers.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
