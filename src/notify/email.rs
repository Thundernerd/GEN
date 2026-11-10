//! Email notifications over SMTP (lettre).

use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::{NotifyContext, NotifyError, TextMessage, engine_names, kind_label, prefixed_title};
use crate::config::{ChannelTarget, SourcesConfig};
use crate::fetch::truncate;
use crate::model::Item;

/// Port on which SMTP uses implicit TLS instead of STARTTLS.
const SMTPS_PORT: u16 = 465;
const MAX_SUMMARY: usize = 1000;

/// A rendered email, independent of transport.
#[derive(Debug, Clone, PartialEq)]
pub struct Email {
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// `target` is always `ChannelTarget::Email`.
pub async fn send(
    ctx: &NotifyContext<'_>,
    target: &ChannelTarget,
    items: &[Item],
) -> Result<(), NotifyError> {
    let digest = matches!(target, ChannelTarget::Email { digest: true, .. });
    deliver(
        target,
        build_emails(ctx.sources, ctx.base_url, digest, items),
    )
    .await
}

/// Sends a plain message, such as a health alert. `target` is always `ChannelTarget::Email`.
pub async fn send_text(target: &ChannelTarget, message: &TextMessage) -> Result<(), NotifyError> {
    deliver(target, vec![build_text_email(message)]).await
}

/// A plain message as an email: the text and the link.
pub fn build_text_email(message: &TextMessage) -> Email {
    let link = escape_html(&message.link);
    Email {
        subject: message.title.clone(),
        text: format!("{}\n\n{}\n", message.text, message.link),
        html: format!(
            "<p>{}</p>\n<p><a href=\"{link}\">{link}</a></p>\n",
            escape_html(&message.text)
        ),
    }
}

async fn deliver(target: &ChannelTarget, emails: Vec<Email>) -> Result<(), NotifyError> {
    let ChannelTarget::Email {
        smtp_host,
        smtp_port,
        username,
        password,
        from,
        to,
        ..
    } = target
    else {
        return Err(NotifyError::Other("not an email channel".into()));
    };
    let from = parse_mailbox("from", from)?;
    let to = to
        .iter()
        .map(|a| parse_mailbox("to", a))
        .collect::<Result<Vec<_>, _>>()?;
    if to.is_empty() {
        return Err(NotifyError::Other(
            "email channel has no `to` addresses".into(),
        ));
    }

    let smtp_err = |e: lettre::transport::smtp::Error| {
        NotifyError::Other(format!("smtp {smtp_host}:{smtp_port}: {e}"))
    };
    let builder = if *smtp_port == SMTPS_PORT {
        AsyncSmtpTransport::<Tokio1Executor>::relay(smtp_host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(smtp_host)
    }
    .map_err(smtp_err)?
    .port(*smtp_port);
    let builder = match (username, password) {
        (Some(u), Some(p)) => builder.credentials(Credentials::new(u.clone(), p.clone())),
        _ => builder,
    };
    let transport = builder.build();

    for email in emails {
        let mut message = Message::builder().from(from.clone());
        for rcpt in &to {
            message = message.to(rcpt.clone());
        }
        let message = message
            .subject(email.subject)
            .multipart(MultiPart::alternative_plain_html(email.text, email.html))
            .map_err(|e| NotifyError::Other(format!("building email: {e}")))?;
        transport.send(message).await.map_err(smtp_err)?;
    }
    Ok(())
}

fn parse_mailbox(field: &str, address: &str) -> Result<Mailbox, NotifyError> {
    address
        .parse()
        .map_err(|e| NotifyError::Other(format!("invalid `{field}` address {address:?}: {e}")))
}

/// One digest email, or one email per item.
pub fn build_emails(
    sources: &SourcesConfig,
    base_url: &str,
    digest: bool,
    items: &[Item],
) -> Vec<Email> {
    if items.is_empty() {
        return Vec::new();
    }
    if !digest {
        return items
            .iter()
            .map(|item| {
                render(
                    prefixed_title(sources, item),
                    sources,
                    base_url,
                    std::slice::from_ref(item),
                )
            })
            .collect();
    }
    let mut names: Vec<String> = Vec::new();
    for name in items.iter().flat_map(|i| engine_names(sources, i)) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let count = match items.len() {
        1 => "1 new item".to_string(),
        n => format!("{n} new items"),
    };
    let subject = if names.is_empty() {
        format!("GEN: {count}")
    } else {
        format!("GEN: {count} ({})", names.join(", "))
    };
    vec![render(subject, sources, base_url, items)]
}

fn render(subject: String, sources: &SourcesConfig, base_url: &str, items: &[Item]) -> Email {
    let mut text = String::new();
    let mut html = String::from(
        "<!DOCTYPE html><html><body style=\"font-family:sans-serif;line-height:1.4\">\n",
    );
    for item in items {
        let names = engine_names(sources, item);
        let meta = if names.is_empty() {
            kind_label(item.kind).to_string()
        } else {
            format!("{} · {}", names.join(", "), kind_label(item.kind))
        };
        let date = item.date().format("%Y-%m-%d %H:%M UTC").to_string();
        let summary = item
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| truncate(s, MAX_SUMMARY));
        // Only link web URLs, never e.g. `javascript:`.
        let url = item
            .url
            .as_deref()
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"));

        text.push_str(&format!("{}\n{meta} · {date}\n", item.title));
        if let Some(url) = url {
            text.push_str(&format!("{url}\n"));
        }
        if let Some(s) = &summary {
            text.push_str(&format!("\n{s}\n"));
        }
        text.push('\n');

        let title = escape_html(&item.title);
        html.push_str("<div style=\"margin-bottom:1.5em\">\n<h3 style=\"margin:0\">");
        match url {
            Some(url) => html.push_str(&format!("<a href=\"{}\">{title}</a>", escape_html(url))),
            None => html.push_str(&title),
        }
        html.push_str(&format!(
            "</h3>\n<div style=\"color:#666;font-size:0.9em\">{} · {date}</div>\n",
            escape_html(&meta)
        ));
        if let Some(s) = &summary {
            // Summaries keep their paragraphs and list items on lines of their own.
            html.push_str(&format!(
                "<p>{}</p>\n",
                escape_html(s).replace('\n', "<br>")
            ));
        }
        html.push_str("</div>\n");
    }
    text.push_str(&format!("-- \nGEN: {base_url}\n"));
    let base = escape_html(base_url);
    html.push_str(&format!(
        "<p style=\"color:#666;font-size:0.8em\">Sent by <a href=\"{base}\">GEN</a></p>\n</body></html>\n"
    ));
    Email {
        subject,
        text,
        html,
    }
}

/// Escapes text for use in HTML content and double- or single-quoted attributes.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;
    use crate::notify::tests::{item, sources};

    const BASE: &str = "https://gen.example";

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape_html(r#"<a href="x">Tom & 'Jerry'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn digest_subject_and_body() {
        let items = vec![
            item(&["godot"], ItemKind::Release, "Godot 4.8", Some("Notes")),
            item(
                &["unity", "godot"],
                ItemKind::News,
                "Engines <compared>",
                None,
            ),
            item(&[], ItemKind::Video, "Roundup", None),
        ];
        let emails = build_emails(&sources(), BASE, true, &items);
        assert_eq!(emails.len(), 1);
        let e = &emails[0];
        assert_eq!(e.subject, "GEN: 3 new items (Godot, Unity)");
        assert!(e.text.contains("Godot 4.8\nGodot · Release · "));
        assert!(
            e.text
                .contains("https://godotengine.org/releases/4.8/\n\nNotes\n")
        );
        assert!(
            e.html
                .contains("<a href=\"https://godotengine.org/releases/4.8/\">Godot 4.8</a>")
        );
        assert!(e.html.contains("Engines &lt;compared&gt;"));
        assert!(!e.html.contains("<compared>"));
        assert!(e.html.contains("Unity, Godot · News"));
        assert!(e.text.ends_with("GEN: https://gen.example\n"));
    }

    #[test]
    fn per_item_emails() {
        let items = vec![
            item(&["godot"], ItemKind::Release, "Godot 4.8", None),
            item(&[], ItemKind::News, "Untagged", None),
        ];
        let subjects: Vec<_> = build_emails(&sources(), BASE, false, &items)
            .into_iter()
            .map(|e| e.subject)
            .collect();
        assert_eq!(subjects, vec!["[Godot] Godot 4.8", "Untagged"]);
        assert!(build_emails(&sources(), BASE, true, &[]).is_empty());
    }

    #[test]
    fn single_item_digest_and_unsafe_url() {
        let mut i = item(&[], ItemKind::News, "X", None);
        i.url = Some("javascript:alert(1)".into());
        let e = &build_emails(&sources(), BASE, true, &[i])[0];
        assert_eq!(e.subject, "GEN: 1 new item");
        assert!(!e.html.contains("javascript:"));
        assert!(!e.text.contains("javascript:"));
    }

    #[test]
    fn mailbox_errors_are_clear() {
        assert!(parse_mailbox("from", "GEN <gen@example.com>").is_ok());
        let err = parse_mailbox("to", "not an address")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("invalid `to` address \"not an address\""),
            "{err}"
        );
    }
}
