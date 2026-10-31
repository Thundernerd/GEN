//! Which channels `/gen setup` creates, and creating them without duplicating channels that
//! already exist. Kept free of Discord types so it can be tested.

use crate::config::BotSubscription;
use crate::model::Topic;

/// Category for layouts that aren't per engine.
pub const SHARED_CATEGORY: &str = "Game Engine News";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// A category per engine with a channel per topic: Unity › #unity-releases.
    CategoryPerEngine,
    /// One channel per engine holding all its topics: #unity.
    ChannelPerEngine,
    /// One channel per topic holding every engine: #releases.
    ChannelPerTopic,
}

impl Layout {
    pub const ALL: [Layout; 3] = [
        Layout::CategoryPerEngine,
        Layout::ChannelPerEngine,
        Layout::ChannelPerTopic,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Layout::CategoryPerEngine => "category",
            Layout::ChannelPerEngine => "engine",
            Layout::ChannelPerTopic => "topic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Layout::CategoryPerEngine => "A category per engine, a channel per topic",
            Layout::ChannelPerEngine => "One channel per engine",
            Layout::ChannelPerTopic => "One channel per topic, all engines together",
        }
    }

    pub fn example(self) -> &'static str {
        match self {
            Layout::CategoryPerEngine => "Unity › #unity-releases, #unity-news",
            Layout::ChannelPerEngine => "Game Engine News › #unity, #godot",
            Layout::ChannelPerTopic => "Game Engine News › #releases, #news",
        }
    }

    pub fn parse(key: &str) -> Option<Layout> {
        Layout::ALL.into_iter().find(|l| l.key() == key)
    }
}

/// An engine to set up: its id (used in channel names) and display name.
#[derive(Debug, Clone)]
pub struct Engine {
    pub id: String,
    pub name: String,
}

/// A channel the layout wants, with what it subscribes to.
#[derive(Debug, Clone, PartialEq)]
pub struct Wanted {
    pub category: String,
    pub name: String,
    /// The channel's description (topic) in Discord.
    pub description: String,
    pub subscriptions: Vec<BotSubscription>,
}

/// The channels for these engines and topics.
pub fn plan(engines: &[Engine], topics: &[Topic], layout: Layout) -> Vec<Wanted> {
    let sub = |e: &Engine, t: Topic| BotSubscription {
        engine: e.id.clone(),
        topic: t,
    };
    let topic_words = || {
        topics
            .iter()
            .map(|t| t.slug())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match layout {
        Layout::CategoryPerEngine => engines
            .iter()
            .flat_map(|e| {
                topics.iter().map(move |&t| Wanted {
                    category: e.name.clone(),
                    name: channel_name(&format!("{}-{}", e.id, t.slug())),
                    description: format!("{} {} from GEN", e.name, t.slug()),
                    subscriptions: vec![sub(e, t)],
                })
            })
            .collect(),
        Layout::ChannelPerEngine => engines
            .iter()
            .map(|e| Wanted {
                category: SHARED_CATEGORY.into(),
                name: channel_name(&e.id),
                description: format!("{} {} from GEN", e.name, topic_words()),
                subscriptions: topics.iter().map(|&t| sub(e, t)).collect(),
            })
            .collect(),
        Layout::ChannelPerTopic => topics
            .iter()
            .map(|&t| {
                let names: Vec<_> = engines.iter().map(|e| e.name.as_str()).collect();
                Wanted {
                    category: SHARED_CATEGORY.into(),
                    name: channel_name(t.slug()),
                    description: format!("{} {} from GEN", names.join(", "), t.slug()),
                    subscriptions: engines.iter().map(|e| sub(e, t)).collect(),
                }
            })
            .collect(),
    }
}

/// A Discord text channel name: lowercase, dashes for anything but letters and digits.
pub fn channel_name(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_end_matches('-').chars().take(100).collect();
    if out.is_empty() { "gen".into() } else { out }
}

/// A channel of the server, as far as setting up cares.
#[derive(Debug, Clone)]
pub struct Existing {
    pub id: u64,
    pub name: String,
    pub is_category: bool,
    pub parent: Option<u64>,
}

/// Creates channels on the server; implemented with serenity, and faked in tests.
pub trait Server {
    async fn create_category(&mut self, name: &str) -> anyhow::Result<u64>;
    async fn create_text_channel(
        &mut self,
        name: &str,
        parent: u64,
        description: &str,
    ) -> anyhow::Result<u64>;
}

/// A channel set up for a [`Wanted`] one.
#[derive(Debug, Clone, PartialEq)]
pub struct Ready {
    pub id: u64,
    pub name: String,
    pub created: bool,
    pub subscriptions: Vec<BotSubscription>,
}

/// Finds or creates each wanted channel and its category. Existing ones are matched by
/// name (ignoring case), so running setup again creates nothing new. Stops at the first
/// error, returning it with the channels that are ready so far (so those still get
/// subscribed).
pub async fn apply(
    server: &mut impl Server,
    existing: &mut Vec<Existing>,
    wanted: &[Wanted],
) -> (Vec<Ready>, Option<anyhow::Error>) {
    let mut out = Vec::new();
    for w in wanted {
        match apply_one(server, existing, w).await {
            Ok(ready) => out.push(ready),
            Err(err) => return (out, Some(err)),
        }
    }
    (out, None)
}

async fn apply_one(
    server: &mut impl Server,
    existing: &mut Vec<Existing>,
    w: &Wanted,
) -> anyhow::Result<Ready> {
    let category = match existing
        .iter()
        .find(|c| c.is_category && c.name.eq_ignore_ascii_case(&w.category))
    {
        Some(c) => c.id,
        None => {
            let id = server.create_category(&w.category).await?;
            existing.push(Existing {
                id,
                name: w.category.clone(),
                is_category: true,
                parent: None,
            });
            id
        }
    };
    let found = existing.iter().find(|c| {
        !c.is_category && c.parent == Some(category) && c.name.eq_ignore_ascii_case(&w.name)
    });
    let (id, created) = match found {
        Some(c) => (c.id, false),
        None => {
            let id = server
                .create_text_channel(&w.name, category, &w.description)
                .await?;
            existing.push(Existing {
                id,
                name: w.name.clone(),
                is_category: false,
                parent: Some(category),
            });
            (id, true)
        }
    };
    Ok(Ready {
        id,
        name: w.name.clone(),
        created,
        subscriptions: w.subscriptions.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engines() -> Vec<Engine> {
        [("unity", "Unity"), ("godot", "Godot")]
            .map(|(id, name)| Engine {
                id: id.into(),
                name: name.into(),
            })
            .to_vec()
    }

    #[derive(Default)]
    struct FakeServer {
        next: u64,
        calls: Vec<String>,
    }

    impl Server for FakeServer {
        async fn create_category(&mut self, name: &str) -> anyhow::Result<u64> {
            self.next += 1;
            self.calls.push(format!("category {name}"));
            Ok(self.next)
        }

        async fn create_text_channel(
            &mut self,
            name: &str,
            parent: u64,
            _description: &str,
        ) -> anyhow::Result<u64> {
            self.next += 1;
            self.calls.push(format!("channel {name} in {parent}"));
            Ok(self.next)
        }
    }

    #[test]
    fn plans_each_layout() {
        let topics = [Topic::Releases, Topic::News];
        let names = |layout| {
            plan(&engines(), &topics, layout)
                .into_iter()
                .map(|w| format!("{}/{} {}", w.category, w.name, w.subscriptions.len()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(Layout::CategoryPerEngine),
            [
                "Unity/unity-releases 1",
                "Unity/unity-news 1",
                "Godot/godot-releases 1",
                "Godot/godot-news 1"
            ]
        );
        assert_eq!(
            names(Layout::ChannelPerEngine),
            ["Game Engine News/unity 2", "Game Engine News/godot 2"]
        );
        assert_eq!(
            names(Layout::ChannelPerTopic),
            ["Game Engine News/releases 2", "Game Engine News/news 2"]
        );
        let first = &plan(&engines(), &topics, Layout::CategoryPerEngine)[0];
        assert_eq!(first.description, "Unity releases from GEN");
        assert_eq!(first.subscriptions[0].topic, Topic::Releases);
    }

    #[test]
    fn channel_names_follow_discord_rules() {
        assert_eq!(channel_name("unreal-releases"), "unreal-releases");
        assert_eq!(channel_name("Cry Engine!-news"), "cry-engine-news");
        assert_eq!(channel_name("--"), "gen");
        assert_eq!(channel_name(&"a".repeat(150)).len(), 100);
    }

    #[tokio::test]
    async fn reuses_existing_channels_and_is_idempotent() {
        let wanted = plan(&engines(), &[Topic::Releases], Layout::CategoryPerEngine);
        // The server already has a Unity category with #Unity-Releases in it, and a
        // #godot-releases outside any matching category.
        let mut existing = vec![
            Existing {
                id: 100,
                name: "UNITY".into(),
                is_category: true,
                parent: None,
            },
            Existing {
                id: 101,
                name: "Unity-Releases".into(),
                is_category: false,
                parent: Some(100),
            },
            Existing {
                id: 102,
                name: "godot-releases".into(),
                is_category: false,
                parent: None,
            },
        ];
        let mut server = FakeServer::default();
        let (ready, error) = apply(&mut server, &mut existing, &wanted).await;
        assert!(error.is_none());
        assert_eq!(
            server.calls,
            ["category Godot", "channel godot-releases in 1"]
        );
        assert_eq!((ready[0].id, ready[0].created), (101, false));
        assert_eq!((ready[1].id, ready[1].created), (2, true));

        let (again, _) = apply(&mut server, &mut existing, &wanted).await;
        assert_eq!(server.calls.len(), 2, "nothing new created");
        assert!(again.iter().all(|r| !r.created));
        assert_eq!(again[1].id, 2);
    }
}
