//! The live set of engines, outlets and sources. It starts from `sources.toml` and is
//! shared by the web UI, the pipeline and the scheduler. Each change publishes a new
//! [`Snapshot`]; readers take the current one and subscribers (the scheduler) are woken.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::watch;

use crate::classify::Classifier;
use crate::config::{Owner, SourcesConfig};
use crate::model::Item;

/// One consistent view of the sources, with everything derived from it.
pub struct Snapshot {
    pub config: SourcesConfig,
    pub classifier: Classifier,
    pub lookup: Lookup,
}

impl Snapshot {
    fn build(config: SourcesConfig) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Snapshot {
            classifier: Classifier::new(&config),
            lookup: Lookup::new(&config),
            config,
        })
    }
}

pub struct SourceRegistry {
    tx: watch::Sender<Arc<Snapshot>>,
}

impl SourceRegistry {
    pub fn new(config: SourcesConfig) -> anyhow::Result<Self> {
        let (tx, _) = watch::channel(Arc::new(Snapshot::build(config)?));
        Ok(SourceRegistry { tx })
    }

    /// The current snapshot. Cheap; hold it for one request or poll, not across changes.
    pub fn current(&self) -> Arc<Snapshot> {
        self.tx.borrow().clone()
    }

    /// A receiver that is marked changed whenever a new snapshot is published.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.tx.subscribe()
    }
}

/// Name lookups derived from a snapshot of the sources.
pub struct Lookup {
    /// Source id -> owner display name, for every source (including disabled ones).
    pub source_names: HashMap<String, String>,
    /// Ids of sources that belong to general outlets rather than an engine.
    pub outlet_sources: HashSet<String>,
    /// Engine id -> display name.
    pub engine_names: HashMap<String, String>,
}

impl Lookup {
    pub fn new(sources: &SourcesConfig) -> Self {
        let all = sources.all_sources();
        Lookup {
            source_names: all
                .iter()
                .map(|s| (s.id.clone(), s.owner_name.clone()))
                .collect(),
            outlet_sources: all
                .iter()
                .filter(|s| matches!(s.owner, Owner::Outlet(_)))
                .map(|s| s.id.clone())
                .collect(),
            engine_names: sources
                .engines
                .iter()
                .map(|e| (e.id.clone(), e.name.clone()))
                .collect(),
        }
    }

    /// Display name of a source's owner, falling back to the raw id for unknown sources.
    pub fn source_name(&self, source_id: &str) -> String {
        self.source_names
            .get(source_id)
            .cloned()
            .unwrap_or_else(|| source_id.to_string())
    }

    pub fn engine_name(&self, engine_id: &str) -> String {
        self.engine_names
            .get(engine_id)
            .cloned()
            .unwrap_or_else(|| engine_id.to_string())
    }

    /// Distinct owner names of the given duplicate sources, minus the item's own owner.
    pub fn also_on(&self, item: &Item, dup_sources: Option<&Vec<String>>) -> Vec<String> {
        let own = self.source_name(&item.source_id);
        let mut names: Vec<String> = Vec::new();
        for id in dup_sources.into_iter().flatten() {
            let name = self.source_name(id);
            if name != own && !names.contains(&name) {
                names.push(name);
            }
        }
        names
    }
}
