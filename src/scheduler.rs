use anyhow::{Context, Result};
use chrono::{DateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Schedule mode for a trigger
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    /// Fire at a specific ISO timestamp, then never again
    Once { at: DateTime<Utc> },
    /// Fire daily at HH:MM in the given timezone
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
    #[serde(default)]
    pub last_fired: Option<DateTime<Utc>>,
}

fn default_enabled() -> bool {
    true
}

impl TriggerRecord {
    /// Compute the next fire time from now
    pub fn next_fire(&self) -> Option<DateTime<Utc>> {
        if !self.enabled {
            return None;
        }

        let now = Utc::now();

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
            Schedule::Daily { time, .. } => {
                // Parse HH:MM
                let target = NaiveTime::parse_from_str(time, "%H:%M").ok()?;
                let today = now.date_naive().and_time(target);
                let today_utc = DateTime::<Utc>::from_naive_utc_and_offset(today, Utc);

                if today_utc > now {
                    Some(today_utc)
                } else {
                    // Tomorrow
                    Some(today_utc + chrono::Duration::days(1))
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
