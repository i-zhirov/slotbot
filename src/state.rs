use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::slots::AvailableSlot;

/// Identity of a previously seen slot, used for de-duplication.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SlotKey {
    pub office_id: String,
    pub service_id: String,
    pub date: String,
    pub time: String,
}

impl SlotKey {
    pub fn from_slot(s: &AvailableSlot) -> Self {
        SlotKey {
            office_id: s.office_id.clone(),
            service_id: s.service_id.clone(),
            date: s.date.clone(),
            time: s.time.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StateFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    slots: Vec<SlotKey>,
}

/// Persistent set of slots already notified about.
#[derive(Debug, Default)]
pub struct SeenSlots {
    slots: BTreeSet<SlotKey>,
    exists: bool,
}

impl SeenSlots {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(SeenSlots {
                slots: BTreeSet::new(),
                exists: false,
            });
        }
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read state file {}", path.display()))?;
        let parsed: StateFile = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse state file {}", path.display()))?;
        Ok(SeenSlots {
            slots: parsed.slots.into_iter().collect(),
            exists: true,
        })
    }

    pub fn contains(&self, key: &SlotKey) -> bool {
        self.slots.contains(key)
    }

    /// True when the state file existed on load (i.e. this is not the first run).
    pub fn existed_before(&self) -> bool {
        self.exists
    }

    pub fn insert(&mut self, key: SlotKey) {
        self.slots.insert(key);
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let data = StateFile {
            version: 1,
            slots: self.slots.iter().cloned().collect(),
        };
        let json = serde_json::to_string_pretty(&data)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
        }
        std::fs::write(path, json)
            .with_context(|| format!("failed to write state file {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(date: &str, time: &str) -> SlotKey {
        SlotKey {
            office_id: "o".into(),
            service_id: "s".into(),
            date: date.into(),
            time: time.into(),
        }
    }

    #[test]
    fn roundtrip_and_contains() {
        let dir = std::env::temp_dir().join(format!("invitro-bot-test-{}", std::process::id()));
        let path = dir.join("state.json");
        let _ = std::fs::remove_file(&path);

        let mut seen = SeenSlots::load(&path).unwrap();
        assert!(!seen.existed_before());
        assert!(!seen.contains(&key("2026-10-05", "10:00")));

        seen.insert(key("2026-10-05", "10:00"));
        seen.save(&path).unwrap();

        let reloaded = SeenSlots::load(&path).unwrap();
        assert!(reloaded.existed_before());
        assert!(reloaded.contains(&key("2026-10-05", "10:00")));
        assert!(!reloaded.contains(&key("2026-10-06", "10:00")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
