//! The `/gen` slash command, its setup panel and the remove confirmation. Every reply is
//! ephemeral: only the admin who ran the command sees it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serenity::all::{
    ButtonStyle, ChannelId, ChannelType, Command, CommandInteraction, CommandOptionType,
    ComponentInteraction, ComponentInteractionDataKind, Context, CreateActionRow,
    CreateAutocompleteResponse, CreateButton, CreateChannel, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateSelectMenu,
    CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse, GuildId, Http,
    Interaction, InteractionContext, Permissions, ResolvedOption, ResolvedValue,
};

use super::Handler;
use super::layout::{self, Layout};
use super::subs::{self, DiscordChannel};
use crate::config::BotSubscription;
use crate::model::Topic;

/// Discord allows 25 options in a select menu (and as autocomplete suggestions).
const MAX_OPTIONS: usize = 25;
/// Interaction tokens last 15 minutes, so a panel can't be used for longer.
const PANEL_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// Open setup panels and remove confirmations with when they were opened, by the id of
/// the command that opened them.
#[derive(Default)]
pub(super) struct Panels(Mutex<HashMap<u64, (Panel, Instant)>>);

#[derive(Debug, Clone)]
enum Panel {
    Setup(SetupPanel),
    /// Channels the bot created that `/gen remove` left without subscriptions.
    Delete {
        guild_id: u64,
        channels: Vec<u64>,
    },
}

#[derive(Debug, Clone)]
struct SetupPanel {
    guild_id: u64,
    /// Selected engine ids.
    engines: Vec<String>,
    /// The engine came with the command, so there is no engine menu.
    fixed_engine: bool,
    topics: Vec<Topic>,
    layout: Layout,
}

impl Panels {
    fn insert(&self, id: u64, panel: Panel) {
        if let Ok(mut panels) = self.0.lock() {
            panels.retain(|_, (_, opened)| opened.elapsed() < PANEL_LIFETIME);
            panels.insert(id, (panel, Instant::now()));
        }
    }

    fn get(&self, id: u64) -> Option<Panel> {
        let panels = self.0.lock().ok()?;
        panels
            .get(&id)
            .filter(|(_, opened)| opened.elapsed() < PANEL_LIFETIME)
            .map(|(p, _)| p.clone())
    }

    fn remove(&self, id: u64) {
        if let Ok(mut panels) = self.0.lock() {
            panels.remove(&id);
        }
    }
}

/// Registers `/gen`, on the dev server only when one is set (instant), else globally.
pub(super) async fn register(ctx: &Context, dev_guild_id: Option<u64>) -> anyhow::Result<()> {
    let commands = vec![command()];
    match dev_guild_id {
        Some(id) => {
            GuildId::new(id).set_commands(&ctx.http, commands).await?;
            tracing::info!(guild = id, "registered discord commands on the dev server");
        }
        None => {
            Command::set_global_commands(&ctx.http, commands).await?;
            tracing::info!("registered discord commands");
        }
    }
    Ok(())
}

fn command() -> CreateCommand {
    let engine = |required| {
        CreateCommandOption::new(CommandOptionType::String, "engine", "Game engine")
            .set_autocomplete(true)
            .required(required)
    };
    let topic = || {
        Topic::ALL
            .into_iter()
            .fold(
                CreateCommandOption::new(CommandOptionType::String, "topic", "What to post")
                    .required(true),
                |o, t| o.add_string_choice(t.label(), t.slug()),
            )
            .add_string_choice("Everything", "all")
    };
    let channel = || {
        CreateCommandOption::new(
            CommandOptionType::Channel,
            "channel",
            "Channel to post in (default: this one)",
        )
        .channel_types(vec![ChannelType::Text, ChannelType::News])
    };
    let sub = |name: &str, description: &str| {
        CreateCommandOption::new(CommandOptionType::SubCommand, name, description)
    };
    CreateCommand::new("gen")
        .description("Game engine news from GEN")
        .default_member_permissions(Permissions::MANAGE_CHANNELS)
        .contexts(vec![InteractionContext::Guild])
        .add_option(
            sub(
                "setup",
                "Create channels for engine news and subscribe them",
            )
            .add_sub_option(engine(false)),
        )
        .add_option(
            sub("subscribe", "Post an engine's news in a channel")
                .add_sub_option(engine(true))
                .add_sub_option(topic())
                .add_sub_option(channel()),
        )
        .add_option(
            sub("unsubscribe", "Stop posting an engine's news in a channel")
                .add_sub_option(engine(true))
                .add_sub_option(topic())
                .add_sub_option(channel()),
        )
        .add_option(sub("list", "Show what is posted where on this server"))
        .add_option(
            sub(
                "remove",
                "Stop posting an engine's news anywhere on this server",
            )
            .add_sub_option(engine(true)),
        )
}

pub(super) async fn handle(handler: &Handler, ctx: &Context, interaction: Interaction) {
    let result = match interaction {
        Interaction::Command(cmd) if cmd.data.name == "gen" => {
            run_command(handler, ctx, &cmd).await
        }
        Interaction::Autocomplete(cmd) if cmd.data.name == "gen" => {
            autocomplete(handler, ctx, &cmd).await
        }
        Interaction::Component(c) if c.data.custom_id.starts_with("gen:") => {
            run_component(handler, ctx, &c).await
        }
        _ => Ok(()),
    };
    if let Err(err) = result {
        tracing::error!("discord interaction failed: {err:#}");
    }
}

fn engine_name(handler: &Handler, id: &str) -> String {
    handler
        .pipeline
        .registry
        .current()
        .config
        .engine(id)
        .map_or_else(|| id.to_string(), |e| e.name.clone())
}

/// Text for a subscription, e.g. "Unity releases".
fn describe(handler: &Handler, sub: &BotSubscription) -> String {
    format!("{} {}", engine_name(handler, &sub.engine), sub.topic.slug())
}

fn describe_all(handler: &Handler, subs: &[BotSubscription]) -> String {
    subs.iter()
        .map(|s| describe(handler, s))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Enabled engines as (id, name).
fn engines(handler: &Handler) -> Vec<(String, String)> {
    handler
        .pipeline
        .registry
        .current()
        .config
        .enabled_engines()
        .map(|e| (e.id.clone(), e.name.clone()))
        .collect()
}

async fn reply(
    ctx: &Context,
    cmd: &CommandInteraction,
    message: CreateInteractionResponseMessage,
) -> anyhow::Result<()> {
    cmd.create_response(
        &ctx.http,
        CreateInteractionResponse::Message(message.ephemeral(true)),
    )
    .await?;
    Ok(())
}

async fn say(
    ctx: &Context,
    cmd: &CommandInteraction,
    text: impl Into<String>,
) -> anyhow::Result<()> {
    reply(
        ctx,
        cmd,
        CreateInteractionResponseMessage::new().content(text),
    )
    .await
}

fn string_option<'a>(options: &'a [ResolvedOption], name: &str) -> Option<&'a str> {
    options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| match o.value {
            ResolvedValue::String(s) => Some(s),
            _ => None,
        })
}

async fn run_command(
    handler: &Handler,
    ctx: &Context,
    cmd: &CommandInteraction,
) -> anyhow::Result<()> {
    let Some(guild_id) = cmd.guild_id else {
        return say(ctx, cmd, "Use this command in a server.").await;
    };
    let options = cmd.data.options();
    let Some(ResolvedOption {
        name,
        value: ResolvedValue::SubCommand(options),
        ..
    }) = options.first()
    else {
        return say(ctx, cmd, "Choose a subcommand.").await;
    };
    let guild_id = guild_id.get();
    let engine = string_option(options, "engine");
    if let Some(id) = engine
        && !engines(handler).iter().any(|(e, _)| e == id)
    {
        return say(
            ctx,
            cmd,
            format!("GEN doesn't follow an engine `{id}`. Pick one from the list."),
        )
        .await;
    }
    match *name {
        "setup" => setup(handler, ctx, cmd, guild_id, engine).await,
        "subscribe" | "unsubscribe" => {
            let engine = engine.unwrap_or_default().to_string();
            let topics: Vec<Topic> = match string_option(options, "topic") {
                Some("all") | None => Topic::ALL.to_vec(),
                Some(t) => Topic::parse(t).into_iter().collect(),
            };
            let wanted: Vec<BotSubscription> = topics
                .into_iter()
                .map(|topic| BotSubscription {
                    engine: engine.clone(),
                    topic,
                })
                .collect();
            let (channel_id, channel_name) = options
                .iter()
                .find_map(|o| match &o.value {
                    ResolvedValue::Channel(c) if o.name == "channel" => {
                        Some((c.id, c.name.clone().unwrap_or_default()))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| {
                    let name = cmd.channel.as_ref().and_then(|c| c.name.clone());
                    (cmd.channel_id, name.unwrap_or_default())
                });
            if *name == "subscribe" {
                subscribe(
                    handler,
                    ctx,
                    cmd,
                    guild_id,
                    channel_id,
                    channel_name,
                    wanted,
                )
                .await
            } else {
                unsubscribe(handler, ctx, cmd, guild_id, channel_id, wanted).await
            }
        }
        "list" => list(handler, ctx, cmd, guild_id).await,
        "remove" => remove(handler, ctx, cmd, guild_id, engine.unwrap_or_default()).await,
        _ => say(ctx, cmd, "Unknown subcommand.").await,
    }
}

async fn autocomplete(
    handler: &Handler,
    ctx: &Context,
    cmd: &CommandInteraction,
) -> anyhow::Result<()> {
    let typed = cmd
        .data
        .autocomplete()
        .filter(|o| o.name == "engine")
        .map(|o| o.value.to_lowercase())
        .unwrap_or_default();
    let choices = engines(handler)
        .into_iter()
        .filter(|(id, name)| id.contains(&typed) || name.to_lowercase().contains(&typed))
        .take(MAX_OPTIONS)
        .fold(CreateAutocompleteResponse::new(), |r, (id, name)| {
            r.add_string_choice(name, id)
        });
    cmd.create_response(&ctx.http, CreateInteractionResponse::Autocomplete(choices))
        .await?;
    Ok(())
}

async fn subscribe(
    handler: &Handler,
    ctx: &Context,
    cmd: &CommandInteraction,
    guild_id: u64,
    channel_id: ChannelId,
    channel_name: String,
    wanted: Vec<BotSubscription>,
) -> anyhow::Result<()> {
    let channel = DiscordChannel {
        guild_id,
        guild_name: handler.guild_name(guild_id),
        channel_id: channel_id.get(),
        channel_name,
        created_by_bot: false,
    };
    let result = handler
        .change("subscribing", move |s| {
            subs::subscribe(s, &channel, &wanted)
        })
        .await;
    let text = match result {
        Ok(added) if added.is_empty() => format!("<#{channel_id}> already gets that."),
        Ok(added) => format!(
            "<#{channel_id}> now gets {}.",
            describe_all(handler, &added)
        ),
        Err(_) => "Saving the subscription failed; see GEN's log.".to_string(),
    };
    say(ctx, cmd, text).await
}

async fn unsubscribe(
    handler: &Handler,
    ctx: &Context,
    cmd: &CommandInteraction,
    guild_id: u64,
    channel_id: ChannelId,
    wanted: Vec<BotSubscription>,
) -> anyhow::Result<()> {
    let id = channel_id.get();
    let result = handler
        .change("unsubscribing", move |s| {
            subs::unsubscribe(s, guild_id, Some(id), |sub| wanted.contains(sub))
        })
        .await;
    let text = match result {
        Ok(removed) if removed.is_empty() => format!("<#{channel_id}> didn't get that."),
        Ok(removed) => {
            let gone: Vec<_> = removed
                .iter()
                .flat_map(|r| r.subscriptions.clone())
                .collect();
            let mut text = format!(
                "<#{channel_id}> no longer gets {}.",
                describe_all(handler, &gone)
            );
            if removed.iter().any(|r| r.emptied) {
                text.push_str(" Nothing is posted there anymore.");
            }
            text
        }
        Err(_) => "Saving the change failed; see GEN's log.".to_string(),
    };
    say(ctx, cmd, text).await
}

async fn list(
    handler: &Handler,
    ctx: &Context,
    cmd: &CommandInteraction,
    guild_id: u64,
) -> anyhow::Result<()> {
    let channels = handler
        .pipeline
        .with_store(move |s| subs::in_guild(s, guild_id))
        .await?;
    let text = if channels.is_empty() {
        "Nothing is posted on this server yet. Use `/gen setup` or `/gen subscribe`.".to_string()
    } else {
        channels
            .iter()
            .map(|c| {
                format!(
                    "<#{}>: {}",
                    c.channel_id,
                    describe_all(handler, &c.subscriptions)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    say(ctx, cmd, truncate_message(&text)).await
}

/// Discord messages hold at most 2000 characters.
fn truncate_message(text: &str) -> String {
    crate::fetch::truncate(text, 1990)
}

async fn remove(
    handler: &Handler,
    ctx: &Context,
    cmd: &CommandInteraction,
    guild_id: u64,
    engine: &str,
) -> anyhow::Result<()> {
    let id = engine.to_string();
    let result = handler
        .change("removing an engine", move |s| {
            subs::unsubscribe(s, guild_id, None, |sub| sub.engine == id)
        })
        .await;
    let removed = match result {
        Ok(removed) => removed,
        Err(_) => return say(ctx, cmd, "Saving the change failed; see GEN's log.").await,
    };
    let name = engine_name(handler, engine);
    if removed.is_empty() {
        return say(
            ctx,
            cmd,
            format!("Nothing on this server gets {name} news."),
        )
        .await;
    }
    let channels: Vec<String> = removed
        .iter()
        .map(|r| format!("<#{}>", r.channel_id))
        .collect();
    let mut text = format!(
        "{name} news is no longer posted in {}.",
        channels.join(", ")
    );
    let deletable: Vec<u64> = removed
        .iter()
        .filter(|r| r.emptied && r.created_by_bot)
        .map(|r| r.channel_id)
        .collect();
    if deletable.is_empty() {
        return say(ctx, cmd, text).await;
    }
    text.push_str(&format!(
        "\n\nGEN created {} of these channels and nothing is posted there anymore. Delete them? \
         An emptied category is deleted too.",
        deletable.len()
    ));
    let panel_id = cmd.id.get();
    handler.panels.insert(
        panel_id,
        Panel::Delete {
            guild_id,
            channels: deletable.clone(),
        },
    );
    let buttons = CreateActionRow::Buttons(vec![
        CreateButton::new(format!("gen:{panel_id}:delete"))
            .style(ButtonStyle::Danger)
            .label(format!("Delete {} channels", deletable.len())),
        CreateButton::new(format!("gen:{panel_id}:keep"))
            .style(ButtonStyle::Secondary)
            .label("Keep them"),
    ]);
    reply(
        ctx,
        cmd,
        CreateInteractionResponseMessage::new()
            .content(truncate_message(&text))
            .components(vec![buttons]),
    )
    .await
}

async fn setup(
    handler: &Handler,
    ctx: &Context,
    cmd: &CommandInteraction,
    guild_id: u64,
    engine: Option<&str>,
) -> anyhow::Result<()> {
    if engines(handler).is_empty() {
        return say(
            ctx,
            cmd,
            "GEN follows no engines yet; add some on its Sources page.",
        )
        .await;
    }
    let panel = SetupPanel {
        guild_id,
        engines: engine.map(str::to_string).into_iter().collect(),
        fixed_engine: engine.is_some(),
        topics: Topic::ALL.to_vec(),
        layout: Layout::CategoryPerEngine,
    };
    let panel_id = cmd.id.get();
    let message = setup_message(handler, panel_id, &panel);
    handler.panels.insert(panel_id, Panel::Setup(panel));
    reply(ctx, cmd, message).await
}

fn setup_message(
    handler: &Handler,
    panel_id: u64,
    panel: &SetupPanel,
) -> CreateInteractionResponseMessage {
    let id = |what: &str| format!("gen:{panel_id}:{what}");
    let all = engines(handler);
    let mut text = String::from("**Set up GEN channels**\n");
    let mut rows = Vec::new();
    if panel.fixed_engine {
        let names: Vec<_> = all
            .iter()
            .filter(|(id, _)| panel.engines.contains(id))
            .map(|(_, name)| name.as_str())
            .collect();
        text.push_str(&format!("Engine: {}\n", names.join(", ")));
    } else {
        // The selected engines first, so they're always among the 25 options.
        let mut shown: Vec<&(String, String)> = all
            .iter()
            .filter(|(id, _)| panel.engines.contains(id))
            .collect();
        shown.extend(all.iter().filter(|(id, _)| !panel.engines.contains(id)));
        shown.truncate(MAX_OPTIONS);
        if all.len() > MAX_OPTIONS {
            text.push_str(&format!(
                "Only {MAX_OPTIONS} engines fit in the menu; use `/gen setup engine:` for the others.\n"
            ));
        }
        let options = shown
            .iter()
            .map(|(id, name)| {
                CreateSelectMenuOption::new(name, id).default_selection(panel.engines.contains(id))
            })
            .collect();
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(id("engines"), CreateSelectMenuKind::String { options })
                .placeholder("Engines")
                .min_values(1)
                .max_values(shown.len() as u8),
        ));
    }
    let topics = Topic::ALL
        .into_iter()
        .map(|t| {
            CreateSelectMenuOption::new(t.label(), t.slug())
                .description(
                    t.kinds()
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
                .default_selection(panel.topics.contains(&t))
        })
        .collect();
    rows.push(CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            id("topics"),
            CreateSelectMenuKind::String { options: topics },
        )
        .placeholder("Topics")
        .min_values(1)
        .max_values(Topic::ALL.len() as u8),
    ));
    let layouts = Layout::ALL
        .into_iter()
        .map(|l| {
            CreateSelectMenuOption::new(l.label(), l.key())
                .description(l.example())
                .default_selection(l == panel.layout)
        })
        .collect();
    rows.push(CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            id("layout"),
            CreateSelectMenuKind::String { options: layouts },
        )
        .min_values(1)
        .max_values(1),
    ));
    rows.push(CreateActionRow::Buttons(vec![
        CreateButton::new(id("create"))
            .style(ButtonStyle::Primary)
            .label("Create channels")
            .disabled(panel.engines.is_empty() || panel.topics.is_empty()),
        CreateButton::new(id("cancel"))
            .style(ButtonStyle::Secondary)
            .label("Cancel"),
    ]));
    text.push_str(
        "Pick engines, topics and how to lay out the channels. Existing channels with the same \
         names are reused.",
    );
    CreateInteractionResponseMessage::new()
        .ephemeral(true)
        .content(text)
        .components(rows)
}

async fn run_component(
    handler: &Handler,
    ctx: &Context,
    c: &ComponentInteraction,
) -> anyhow::Result<()> {
    let mut parts = c.data.custom_id.splitn(3, ':').skip(1);
    let (Some(panel_id), Some(action)) = (
        parts.next().and_then(|p| p.parse::<u64>().ok()),
        parts.next(),
    ) else {
        return Ok(());
    };
    let Some(panel) = handler.panels.get(panel_id) else {
        return update(ctx, c, "This panel has expired. Run the command again.").await;
    };
    let values = match &c.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => values.clone(),
        _ => Vec::new(),
    };
    match (panel, action) {
        (Panel::Setup(mut setup), "engines" | "topics" | "layout") => {
            match action {
                "engines" => setup.engines = values,
                "topics" => setup.topics = values.iter().filter_map(|v| Topic::parse(v)).collect(),
                _ => {
                    setup.layout = values
                        .first()
                        .and_then(|v| Layout::parse(v))
                        .unwrap_or(setup.layout)
                }
            }
            let message = setup_message(handler, panel_id, &setup);
            handler.panels.insert(panel_id, Panel::Setup(setup));
            c.create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(message))
                .await?;
            Ok(())
        }
        (Panel::Setup(setup), "create") => {
            handler.panels.remove(panel_id);
            update(ctx, c, "Setting up channels…").await?;
            let text = match create_channels(handler, &ctx.http, &setup).await {
                Ok(text) => text,
                Err(err) => format!("Setting up failed: {err:#}"),
            };
            edit(ctx, c, &text).await
        }
        (Panel::Delete { guild_id, channels }, "delete") => {
            handler.panels.remove(panel_id);
            update(ctx, c, "Deleting channels…").await?;
            let text = match delete_channels(&ctx.http, guild_id, &channels).await {
                Ok(text) => text,
                Err(err) => format!("Deleting failed: {err:#}"),
            };
            edit(ctx, c, &text).await
        }
        (_, "cancel" | "keep") => {
            handler.panels.remove(panel_id);
            update(ctx, c, "Okay, nothing changed.").await
        }
        _ => Ok(()),
    }
}

/// Replaces the panel with a message.
async fn update(ctx: &Context, c: &ComponentInteraction, text: &str) -> anyhow::Result<()> {
    let message = CreateInteractionResponseMessage::new()
        .content(text)
        .components(Vec::new());
    c.create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(message))
        .await?;
    Ok(())
}

/// Replaces the panel's message after it was updated.
async fn edit(ctx: &Context, c: &ComponentInteraction, text: &str) -> anyhow::Result<()> {
    c.edit_response(
        &ctx.http,
        EditInteractionResponse::new()
            .content(truncate_message(text))
            .components(Vec::new()),
    )
    .await?;
    Ok(())
}

/// Creates channels with serenity.
struct Guild<'a> {
    http: &'a Http,
    id: GuildId,
}

impl layout::Server for Guild<'_> {
    async fn create_category(&mut self, name: &str) -> anyhow::Result<u64> {
        let channel = self
            .id
            .create_channel(
                self.http,
                CreateChannel::new(name).kind(ChannelType::Category),
            )
            .await
            .with_context(|| format!("creating the {name} category"))?;
        Ok(channel.id.get())
    }

    async fn create_text_channel(
        &mut self,
        name: &str,
        parent: u64,
        description: &str,
    ) -> anyhow::Result<u64> {
        let channel = self
            .id
            .create_channel(
                self.http,
                CreateChannel::new(name)
                    .kind(ChannelType::Text)
                    .category(ChannelId::new(parent))
                    .topic(description),
            )
            .await
            .with_context(|| format!("creating #{name}"))?;
        Ok(channel.id.get())
    }
}

/// The server's categories and text channels.
async fn existing_channels(http: &Http, guild: GuildId) -> anyhow::Result<Vec<layout::Existing>> {
    let channels = guild
        .channels(http)
        .await
        .context("listing the server's channels")?;
    Ok(channels
        .values()
        .filter(|c| {
            matches!(
                c.kind,
                ChannelType::Category | ChannelType::Text | ChannelType::News
            )
        })
        .map(|c| layout::Existing {
            id: c.id.get(),
            name: c.name.clone(),
            is_category: c.kind == ChannelType::Category,
            parent: c.parent_id.map(|p| p.get()),
        })
        .collect())
}

async fn create_channels(
    handler: &Handler,
    http: &Http,
    setup: &SetupPanel,
) -> anyhow::Result<String> {
    let guild = GuildId::new(setup.guild_id);
    let engines: Vec<layout::Engine> = engines(handler)
        .into_iter()
        .filter(|(id, _)| setup.engines.contains(id))
        .map(|(id, name)| layout::Engine { id, name })
        .collect();
    let wanted = layout::plan(&engines, &setup.topics, setup.layout);
    let mut existing = existing_channels(http, guild).await?;
    let (ready, error) =
        layout::apply(&mut Guild { http, id: guild }, &mut existing, &wanted).await;

    let guild_name = handler.guild_name(setup.guild_id);
    let mut lines = Vec::new();
    for r in ready {
        let channel = DiscordChannel {
            guild_id: setup.guild_id,
            guild_name: guild_name.clone(),
            channel_id: r.id,
            channel_name: r.name.clone(),
            created_by_bot: r.created,
        };
        let wanted = r.subscriptions.clone();
        handler
            .change("subscribing", move |s| {
                subs::subscribe(s, &channel, &wanted)
            })
            .await?;
        lines.push(format!(
            "<#{}>{}: {}",
            r.id,
            if r.created { " (new)" } else { "" },
            describe_all(handler, &r.subscriptions)
        ));
    }
    let mut text = if lines.is_empty() {
        String::new()
    } else {
        format!("Set up {} channels:\n{}", lines.len(), lines.join("\n"))
    };
    if let Some(err) = error {
        text.push_str(&format!(
            "\n\nStopped: {err:#}. Check that GEN may manage channels here, then run `/gen setup` again."
        ));
    } else {
        text.push_str("\n\nNew items will be posted as they arrive.");
    }
    Ok(text)
}

/// Deletes the channels, then any category they leave empty.
async fn delete_channels(http: &Http, guild_id: u64, channels: &[u64]) -> anyhow::Result<String> {
    let guild = GuildId::new(guild_id);
    let before = existing_channels(http, guild).await?;
    let parents: Vec<u64> = before
        .iter()
        .filter(|c| channels.contains(&c.id))
        .filter_map(|c| c.parent)
        .collect();
    let mut deleted = 0;
    for &id in channels {
        if before.iter().any(|c| c.id == id) {
            ChannelId::new(id)
                .delete(http)
                .await
                .context("deleting a channel")?;
            deleted += 1;
        }
    }
    // Every kind of channel, so a category holding a voice channel counts as in use.
    let after = guild
        .channels(http)
        .await
        .context("listing the server's channels")?;
    let mut categories = 0;
    for parent in parents
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
    {
        let in_use = after
            .values()
            .any(|c| c.parent_id.map(|p| p.get()) == Some(parent));
        if !in_use && after.contains_key(&ChannelId::new(parent)) {
            ChannelId::new(parent)
                .delete(http)
                .await
                .context("deleting a category")?;
            categories += 1;
        }
    }
    Ok(match categories {
        0 => format!("Deleted {deleted} channels."),
        n => format!("Deleted {deleted} channels and {n} empty categories."),
    })
}
