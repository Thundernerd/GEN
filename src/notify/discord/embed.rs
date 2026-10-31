//! Classic embeds: the polished, compact and big-image styles.

use serde_json::{Value, json};

use super::parts::{Parts, escape};
use crate::fetch::truncate;

const MAX_TITLE: usize = 256;
const MAX_AUTHOR: usize = 256;
const MAX_FIELD_VALUE: usize = 1024;
const MAX_FOOTER: usize = 2048;
const POLISHED_DESCRIPTION: usize = 600;
const COMPACT_DESCRIPTION: usize = 160;
const IMAGE_DESCRIPTION: usize = 280;

/// A full embed: engine line with icon, summary with bullets, fields, thumbnail.
/// Returns it with the characters Discord counts toward a message's limit.
pub fn polished(p: &Parts) -> (Value, usize) {
    let mut e = Embed::new(p);
    e.description(p.description(POLISHED_DESCRIPTION));
    for (name, value) in &p.fields {
        e.field(name, value);
    }
    if let Some(url) = &p.thumbnail {
        e.value["thumbnail"] = json!({ "url": url });
    }
    e.footer(&format!("GEN · {}", p.source));
    e.finish()
}

/// A short embed: engine line, title, one short paragraph, source.
pub fn compact(p: &Parts) -> (Value, usize) {
    let mut e = Embed::new(p);
    e.description(p.one_paragraph(COMPACT_DESCRIPTION));
    e.footer(&p.source);
    e.finish()
}

/// An embed led by the item's large preview image. `None` without an image.
pub fn image(p: &Parts) -> Option<(Value, usize)> {
    let url = p.image.as_ref()?;
    let mut e = Embed::new(p);
    e.description(p.description(IMAGE_DESCRIPTION));
    e.value["image"] = json!({ "url": url });
    e.footer(&format!("GEN · {}", p.source));
    Some(e.finish())
}

/// An embed being built, counting its text.
struct Embed {
    value: Value,
    chars: usize,
}

impl Embed {
    /// Engine line, title, link, colour and time: what every style shows.
    fn new(p: &Parts) -> Embed {
        let author = truncate(&p.author, MAX_AUTHOR - 1);
        let title = truncate(&p.title, MAX_TITLE - 1);
        let mut value = json!({
            "author": { "name": author },
            "title": title,
            "color": p.color,
            "timestamp": p.timestamp.to_rfc3339(),
        });
        if let Some(icon) = &p.author_icon {
            value["author"]["icon_url"] = json!(icon);
        }
        if let Some(url) = &p.url {
            value["url"] = json!(url);
        }
        let chars = author.chars().count() + title.chars().count();
        Embed { value, chars }
    }

    fn description(&mut self, text: String) {
        if !text.is_empty() {
            self.chars += text.chars().count();
            self.value["description"] = json!(text);
        }
    }

    fn field(&mut self, name: &str, value: &str) {
        let value = truncate(&escape(value), MAX_FIELD_VALUE - 1);
        self.chars += name.chars().count() + value.chars().count();
        let fields = self.value["fields"].as_array_mut();
        let field = json!({ "name": name, "value": value, "inline": true });
        match fields {
            Some(fields) => fields.push(field),
            None => self.value["fields"] = json!([field]),
        }
    }

    fn footer(&mut self, text: &str) {
        let text = truncate(text, MAX_FOOTER - 1);
        self.chars += text.chars().count();
        self.value["footer"] = json!({ "text": text });
    }

    fn finish(self) -> (Value, usize) {
        (self.value, self.chars)
    }
}
