//! Runtime on/off switches for sources, shared by the web UI (which changes them) and the
//! scheduler (which respects them). Persisted as overrides in the store; a source without
//! an override uses its own `enabled` flag.

use std::collections::HashMap;
use std::sync::RwLock;

use tokio::sync::Notify;

use crate::config::ResolvedSource;

#[derive(Default)]
pub struct SourceToggles {
    overrides: RwLock<HashMap<String, bool>>,
    changed: Notify,
}

impl SourceToggles {
    /// Starts from the overrides loaded from the store.
    pub fn new(overrides: HashMap<String, bool>) -> Self {
        SourceToggles {
            overrides: RwLock::new(overrides),
            changed: Notify::new(),
        }
    }

    pub fn is_enabled(&self, source: &ResolvedSource) -> bool {
        self.overrides
            .read()
            .map(|o| o.get(&source.id).copied())
            .unwrap_or_default()
            .unwrap_or(source.enabled)
    }

    /// Records an override (the caller persists it) and wakes waiting pollers.
    pub fn set(&self, source_id: &str, enabled: bool) {
        if let Ok(mut o) = self.overrides.write() {
            o.insert(source_id.to_string(), enabled);
        }
        self.changed.notify_waiters();
    }

    /// Drops the override of a removed source (the caller deletes the stored one).
    pub fn forget(&self, source_id: &str) {
        if let Ok(mut o) = self.overrides.write() {
            o.remove(source_id);
        }
    }

    /// Resolves the next time any toggle changes.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Owner, SourceSpec};

    fn source(id: &str, enabled: bool) -> ResolvedSource {
        ResolvedSource {
            id: id.into(),
            owner: Owner::Engine("godot".into()),
            owner_name: "Godot".into(),
            enabled,
            category: None,
            interval_minutes: None,
            keywords: Vec::new(),
            require_engine_match: false,
            spec: SourceSpec::Hn {
                query: "godot".into(),
            },
        }
    }

    #[test]
    fn overrides_win_over_config() {
        let toggles = SourceToggles::new(HashMap::from([("a".to_string(), false)]));
        assert!(!toggles.is_enabled(&source("a", true)));
        assert!(toggles.is_enabled(&source("b", true)));
        assert!(!toggles.is_enabled(&source("c", false)));
        toggles.set("c", true);
        assert!(toggles.is_enabled(&source("c", false)));
    }
}
