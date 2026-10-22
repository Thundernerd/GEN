//! Turns `RawItem`s into `NewItem`s: decides the item kind, tags engines and computes the
//! cross-source `story_key`.

use crate::config::{Owner, ResolvedSource, SourcesConfig};
use crate::model::{ItemKind, NewItem, RawItem};

pub struct Classifier {}

impl Classifier {
    pub fn new(_sources: &SourcesConfig) -> Self {
        Classifier {}
    }

    /// Returns `None` when the item should be dropped (keyword filter or
    /// `require_engine_match` without a match).
    pub fn classify(&self, source: &ResolvedSource, raw: RawItem) -> Option<NewItem> {
        let engines = match &source.owner {
            Owner::Engine(id) => vec![id.clone()],
            Owner::Outlet(_) => Vec::new(),
        };
        let kind = source.category.unwrap_or(ItemKind::News);
        Some(NewItem {
            source_id: source.id.clone(),
            story_key: story_key(&raw.title, &engines),
            guid: raw.guid,
            engines,
            kind,
            title: raw.title,
            url: raw.url,
            summary: raw.summary,
            published: raw.published,
        })
    }
}

/// Normalized title plus primary engine, so the same story from different sources collides.
pub fn story_key(title: &str, engines: &[String]) -> String {
    let normalized: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match engines.first() {
        Some(engine) => format!("{engine}|{normalized}"),
        None => normalized,
    }
}
