//! The card style: a message built from Discord's layout components ("Components V2"):
//! a container with the kind's accent colour holding text, a thumbnail, a separator and
//! link buttons.

use serde_json::{Value, json};

use super::parts::{Parts, escape};
use crate::fetch::truncate;

/// Marks a message as made of layout components (`IS_COMPONENTS_V2`). Such a message
/// can't have `content` or `embeds`.
pub const COMPONENTS_V2: u64 = 1 << 15;
const CONTAINER: u8 = 17;
const SECTION: u8 = 9;
const TEXT: u8 = 10;
const THUMBNAIL: u8 = 11;
const SEPARATOR: u8 = 14;
pub const ACTION_ROW: u8 = 1;
const BUTTON: u8 = 2;
const LINK_STYLE: u8 = 5;

const MAX_TITLE: usize = 200;
const DESCRIPTION: usize = 700;
/// Discord allows 5 buttons in a row.
const MAX_BUTTONS: usize = 5;
/// Discord's limit on all text in a message's components; the budgets above stay well
/// under it.
#[cfg(test)]
pub const MAX_TEXT: usize = 4000;

/// The message for one item.
pub fn message(p: &Parts) -> Value {
    let mut inner = vec![text(&format!(
        "-# **{}**",
        escape(&p.author.to_uppercase())
    ))];

    let title = escape(&truncate(&p.title, MAX_TITLE));
    let heading = match &p.url {
        Some(url) => format!("## [{title}]({url})"),
        None => format!("## {title}"),
    };
    let summary = p.description(DESCRIPTION);
    let body = if summary.is_empty() {
        heading
    } else {
        format!("{heading}\n{summary}")
    };
    inner.push(match &p.thumbnail {
        Some(url) => json!({
            "type": SECTION,
            "components": [text(&body)],
            "accessory": { "type": THUMBNAIL, "media": { "url": url } },
        }),
        None => text(&body),
    });

    if !p.fields.is_empty() {
        let facts: Vec<String> = p
            .fields
            .iter()
            .filter(|(name, _)| name != "Source")
            .map(|(name, value)| format!("**{name}** {}", escape(value)))
            .collect();
        if !facts.is_empty() {
            inner.push(text(&facts.join(" · ")));
        }
    }
    inner.push(json!({ "type": SEPARATOR, "divider": true, "spacing": 1 }));
    inner.push(text(&format!(
        "-# {} · <t:{}:R>",
        escape(&p.source),
        p.timestamp.timestamp()
    )));
    if let Some(row) = buttons(p) {
        inner.push(row);
    }
    json!({
        "flags": COMPONENTS_V2,
        "components": [{ "type": CONTAINER, "accent_color": p.color, "components": inner }],
    })
}

/// A row of link buttons, if there are links.
pub fn buttons(p: &Parts) -> Option<Value> {
    let buttons: Vec<Value> = p
        .buttons
        .iter()
        .take(MAX_BUTTONS)
        .map(|(label, url)| json!({ "type": BUTTON, "style": LINK_STYLE, "label": label, "url": url }))
        .collect();
    (!buttons.is_empty()).then(|| json!({ "type": ACTION_ROW, "components": buttons }))
}

fn text(content: &str) -> Value {
    json!({ "type": TEXT, "content": content })
}

/// All text in a message's components, as Discord counts it.
#[cfg(test)]
pub fn text_len(message: &Value) -> usize {
    match message {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| match (k.as_str(), v) {
                ("content" | "label", Value::String(s)) => s.chars().count(),
                _ => text_len(v),
            })
            .sum(),
        Value::Array(items) => items.iter().map(text_len).sum(),
        _ => 0,
    }
}
