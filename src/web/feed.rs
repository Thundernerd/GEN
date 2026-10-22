//! The combined Atom feed.

use atom_syndication::{
    CategoryBuilder, EntryBuilder, Feed, FeedBuilder, FixedDateTime, LinkBuilder, PersonBuilder,
    Text,
};
use chrono::Utc;

use crate::model::Item;

use super::view::Lookup;

/// Builds the feed. `base_url` has no trailing slash; `query` is the filter query string
/// (with leading `?`, or empty) so the feed's own id and self link reflect the filters.
pub fn build(items: &[Item], lookup: &Lookup, base_url: &str, query: &str, title: &str) -> Feed {
    let updated: FixedDateTime = items
        .iter()
        .map(Item::date)
        .max()
        .unwrap_or_else(Utc::now)
        .fixed_offset();
    let self_url = format!("{base_url}/feed.xml{query}");
    let entries = items
        .iter()
        .map(|item| {
            let item_url = format!("{base_url}/#item-{}", item.id);
            let href = item
                .url
                .clone()
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                .unwrap_or_else(|| item_url.clone());
            let categories = item
                .engines
                .iter()
                .map(|e| {
                    CategoryBuilder::default()
                        .term(e.clone())
                        .label(Some(lookup.engine_name(e)))
                        .build()
                })
                .chain(std::iter::once(
                    CategoryBuilder::default()
                        .term(item.kind.as_str().to_string())
                        .scheme(Some(format!("{base_url}/kinds")))
                        .build(),
                ))
                .collect::<Vec<_>>();
            EntryBuilder::default()
                .id(item_url)
                .title(Text::plain(item.title.clone()))
                .updated(item.first_seen.fixed_offset())
                .published(Some(item.date().fixed_offset()))
                .links(vec![
                    LinkBuilder::default()
                        .href(href)
                        .rel("alternate".to_string())
                        .build(),
                ])
                .authors(vec![
                    PersonBuilder::default()
                        .name(lookup.source_name(&item.source_id))
                        .build(),
                ])
                .categories(categories)
                .summary(item.summary.clone().map(Text::plain))
                .build()
        })
        .collect::<Vec<_>>();

    FeedBuilder::default()
        .id(self_url.clone())
        .title(Text::plain(title.to_string()))
        .updated(updated)
        .links(vec![
            LinkBuilder::default()
                .href(self_url)
                .rel("self".to_string())
                .mime_type(Some("application/atom+xml".to_string()))
                .build(),
            LinkBuilder::default()
                .href(format!("{base_url}/"))
                .rel("alternate".to_string())
                .mime_type(Some("text/html".to_string()))
                .build(),
        ])
        .entries(entries)
        .build()
}
