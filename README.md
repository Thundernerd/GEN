# GEN — Game Engine News

GEN tracks new releases and news for game engines and collects them on one page, in one feed and in your notification channels. It focuses on engines you can script in C#:

- **Tracked engines:** Unity, Unreal, Godot, Stride, Flax, S&Box, Prowl, LODZERO and OpenEngine, plus other C#-capable and notable engines.
- **News outlets:** general game-dev outlets such as GameFromScratch. Their articles are tagged with the engines they mention.

GEN is a single Rust binary. It contains a web server and a background poller, and stores its data in SQLite.

## Features

- Polls RSS/Atom/JSON feeds, GitHub releases/tags/commits, the Unity release API, Hacker News searches and scraped HTML pages.
- Classifies each item as `release`, `prerelease`, `news`, `video` or `community`, and tags it with engines using per-engine aliases.
- Removes cross-source duplicates, for example the same release announced on GitHub, the engine blog and GameFromScratch.
- Provides a web UI by engine and by source, a combined Atom feed and a JSON API.
- Sends notifications to Discord, ntfy and email, with per-channel filter rules. Email can be sent as a digest.
- Uses conditional requests (ETag / Last-Modified) and per-source intervals. The first fetch of a new source fills the database without notifying, so adding a source never floods your channels.

### How it works

`gen serve` loads two config files: `gen.toml` for app settings and `sources.toml` for engines, outlets and sources. It then starts an axum HTTP server and a scheduler. The scheduler polls each enabled source on its interval, with a limit on how many fetches run at once. Fetchers turn responses into raw items. The classifier assigns a kind and engine tags, and computes a normalized story key for deduplication. New items go into SQLite, and each item that matches a channel's rules is sent to that channel. The web UI and the feeds read from the same database.

## Quick start (local)

Requires Rust 1.96 or newer (edition 2024). You don't need OpenSSL or a system SQLite: TLS uses rustls, and SQLite is compiled in.

```sh
cargo run -- check-config     # validate config/gen.toml + config/sources.toml, list sources
cargo run -- fetch            # poll every source once and print what's new (no notifications)
cargo run -- fetch --source godot   # only sources whose id contains "godot"
cargo run -- serve            # web server + background polling
```

Then open <http://localhost:8080>. The database is created at `data/gen.db`.

CLI options, which work with every subcommand:

| Option | Env var | Default |
|---|---|---|
| `--config <path>` | `GEN_CONFIG` | `config/gen.toml` |
| `--sources <path>` | `GEN_SOURCES` | `config/sources.toml` |

Logging uses `RUST_LOG`, for example `RUST_LOG=debug` or `RUST_LOG=gen=debug`. The default is `info`.

## Docker

The image is multi-stage. It builds on `rust:1.96-slim-bookworm` and runs on `debian:bookworm-slim` as a non-root user. It contains the binary, `templates/`, `static/`, `config/sources.toml`, and `deploy/gen.docker.toml` as `/app/config/gen.toml`. Data lives in the `/data` volume, and the server listens on port 8080. A healthcheck polls `/health`.

### Compose (recommended)

```sh
cp deploy/.env.example .env      # optional: tokens and notification secrets
$EDITOR deploy/gen.docker.toml   # base_url, notification channels
docker compose up -d --build
docker compose logs -f gen
```

`docker-compose.yml` does the following:

- builds the image and restarts it `unless-stopped`
- publishes `8080:8080`
- stores the database in the named volume `gen-data`, mounted at `/data`
- mounts `deploy/gen.docker.toml` as `/app/config/gen.toml` (read-only)
- mounts `config/sources.toml` over the copy in the image. To pick up source changes, run `docker compose restart gen`; no rebuild is needed. Remove this mount to use the copy baked into the image.
- reads `.env` if it exists, which is optional

Run the other subcommands in the container:

```sh
docker compose run --rm gen gen check-config
docker compose run --rm gen gen fetch
docker compose run --rm gen gen notify-test --channel phone
```

### Plain docker

```sh
docker build -t gen .
docker run -d --name gen -p 8080:8080 -v gen-data:/data --env-file .env gen
```

## Home server with systemd

`deploy/gen.service` runs the bare binary as a dedicated `gen` user. It uses `/opt/gen` as its working directory and `/var/lib/gen` as the only writable path. Hardening options include `ProtectSystem=strict`, `NoNewPrivileges` and `PrivateTmp`.

```sh
cargo build --release

sudo useradd --system --home-dir /opt/gen --shell /usr/sbin/nologin gen
sudo install -d -o gen -g gen /opt/gen /var/lib/gen
sudo install -m 0755 target/release/gen /usr/local/bin/gen
sudo cp -r templates static config /opt/gen/

# Point the database at the writable data dir:
#   [database]
#   path = "/var/lib/gen/gen.db"
sudo $EDITOR /opt/gen/config/gen.toml

# Optional secrets:
sudo install -d /etc/gen
sudo install -m 0600 deploy/.env.example /etc/gen/gen.env
sudo $EDITOR /etc/gen/gen.env

sudo cp deploy/gen.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now gen
journalctl -u gen -f
```

`config/gen.toml` binds to `127.0.0.1:8080` by default. Change `bind` to `0.0.0.0:8080` to reach GEN from other machines, or put a reverse proxy (Caddy, nginx) in front of it, and set `base_url` to the public URL. To update, replace the binary or the files in `/opt/gen`, then run `sudo systemctl restart gen`.

## Configuration

### gen.toml

String values can reference environment variables as `${NAME}`. If a referenced variable is unset, GEN refuses to start, so leave out any line you don't use. When `gen.toml` is missing, the defaults below apply.

```toml
[server]
bind = "127.0.0.1:8080"            # default 0.0.0.0:8080
base_url = "http://localhost:8080" # public URL, used in /feed.xml and notifications
templates_dir = "templates"        # default
static_dir = "static"              # default
# admin_token = "${GEN_ADMIN_TOKEN}" # enables the admin login; see "Managing sources from the web"

[database]
path = "data/gen.db"               # SQLite file; parent dir must be writable

[polling]
default_interval_minutes = 60      # per-source override: interval_minutes
concurrency = 8                    # max sources fetched at the same time
# user_agent = "GEN/0.1.0 (Game Engine News aggregator)"
# github_token = "${GITHUB_TOKEN}" # optional; the GITHUB_TOKEN env var is used automatically

[[notify.channel]]                 # repeat for each channel, see "Notifications"
name = "..."
kind = "discord" | "ntfy" | "email"
```

Relative paths resolve against the working directory. A GitHub token is optional, but without one the GitHub API allows only 60 requests per hour.

### sources.toml: engines and outlets

There are two kinds of owners, and each has one or more `source` entries:

- **`[[engine]]`** is a game engine. Every item from its sources is tagged with that engine. Its `aliases` are also used to tag outlet articles that mention it.
- **`[[outlet]]`** is a general news site, such as GameFromScratch. Its items are tagged with any engine whose alias they mention. With `require_engine_match = true`, items that mention no engine are dropped.

```toml
[[engine]]
id = "stride"                    # unique; used in URLs and notify rules
name = "Stride"
tier = 1                         # 1 = major C#, 2 = niche C#, 3 = notable without C#
csharp = "native"                # free text, shown in the UI
homepage = "https://www.stride3d.net"
enabled = true                   # default true
aliases = ["stride", "stride3d", "stride engine"]   # case-insensitive whole words
exclude = ["stride length"]      # phrases that veto an alias match

  [[engine.source]]
  kind = "feed"
  url = "https://www.stride3d.net/feed.xml"
  category = "news"

  [[engine.source]]
  kind = "github"
  repo = "stride3d/stride"
  mode = "tags"                  # releases (default) | tags | commits
  tag_prefix = "releases/"       # only tags starting with this
  category = "release"
  interval_minutes = 30          # override the default polling interval

[[outlet]]
id = "gamefromscratch"
name = "GameFromScratch"
homepage = "https://gamefromscratch.com"
require_engine_match = false

  [[outlet.source]]
  kind = "feed"
  url = "https://gamefromscratch.com/feed/"
  category = "news"
```

**Source kinds**

| kind | Fields | Notes |
|---|---|---|
| `feed` | `url`, `cache_bust` (bool) | RSS, Atom or JSON Feed. `cache_bust` defeats stale CDN caches. |
| `github` | `repo` (`owner/repo`), `mode`, `tag_prefix` | Releases (with the prerelease flag), tags or commits. |
| `unity` | `limit` (default 25) | Unity editor releases API. |
| `hn` | `query` | Hacker News search via Algolia. |
| `html` | `url`, `item`, `title`, `link`, `date` | Scrapes a page. `item` is a CSS selector matching one element per entry; the others are relative to it. |

All source kinds also accept these fields:

- `category`: the default item kind
- `enabled`: whether the source is polled
- `interval_minutes`: overrides the default polling interval
- `keywords`: keeps only items whose title or summary contains one of them

**Categories / item kinds:** `release`, `prerelease`, `news`, `video`, `community`. The classifier can refine the category. For example, a `release` tagged `-beta` or flagged as a prerelease on GitHub becomes `prerelease`.

**Tiers:**

- Tier 1 covers major or very active engines with C# support, plus Unreal (C# via plugins only): Unity, Unreal, Godot (.NET), Stride, Flax, S&Box, Prowl, OpenEngine, MonoGame, FNA, NeoAxis, Evergine, UNIGINE, Silk.NET and BRUTAL.
- Tier 2 covers niche C# engines, frameworks and libraries, such as LODZERO, KNI, Murder, Raylib-cs, Foster, MonoGame.Extended, Myra, Arch and BEPUphysics.
- Tier 3 covers notable engines without C# support, such as Bevy, Defold, O3DE and GameMaker. Tier-3 engines ship with `enabled = false`, so they are listed but not polled. Set `enabled = true` on any you want to follow.

You can disable a whole engine or outlet, or a single source, with `enabled = false`. Run `gen check-config` after editing.

To add an engine or outlet:

1. Add the `[[engine]]` or `[[outlet]]` block.
2. Run `cargo run -- check-config`.
3. Run `cargo run -- fetch --source <id>` to see what it produces.
4. Restart the server.

New sources fill the database silently on their first successful fetch.

## Notifications

Each `[[notify.channel]]` has a `name`, a `kind`, optional `enabled = false` and an optional `[notify.channel.rules]` table. An empty or missing rule list means "no restriction".

| Rule | Meaning |
|---|---|
| `engines = ["godot", "stride"]` | Only items tagged with one of these engine ids. |
| `kinds = ["release"]` | Only these item kinds. |
| `exclude_kinds = ["prerelease"]` | Never these kinds. |
| `keywords = ["LTS", "security"]` | The title or summary must contain at least one (case-insensitive). |
| `include_untagged = true` | Also send outlet items that matched no engine. Off by default. |

```toml
[[notify.channel]]
name = "discord-releases"
kind = "discord"
webhook_url = "${GEN_DISCORD_WEBHOOK}"
  [notify.channel.rules]
  engines = ["godot", "unity", "stride", "flax"]
  kinds = ["release"]

[[notify.channel]]
name = "phone"
kind = "ntfy"
server = "https://ntfy.sh"       # default; or your own ntfy server
topic = "${GEN_NTFY_TOPIC}"
# token = "${GEN_NTFY_TOKEN}"    # for protected topics / self-hosted servers
  [notify.channel.rules]
  kinds = ["release", "news"]
  exclude_kinds = ["prerelease"]

[[notify.channel]]
name = "mail-digest"
kind = "email"
smtp_host = "smtp.example.com"
smtp_port = 587                  # default; STARTTLS
username = "${GEN_SMTP_USER}"
password = "${GEN_SMTP_PASSWORD}"
from = "GEN <gen@example.com>"
to = ["you@example.com"]
digest = true                    # one mail per poll cycle instead of one per item
```

### Setup tips

- **Discord:** in the channel settings, open Integrations, then Webhooks, then New Webhook, and copy the webhook URL into `GEN_DISCORD_WEBHOOK`. Treat the URL as a secret.
- **ntfy:** choose a long, random topic name, because anyone who knows a topic on ntfy.sh can read it. Subscribe to it in the ntfy app or at `https://ntfy.sh/<topic>`. For a self-hosted server with access control, set `server` and `token`.
- **SMTP:** use port 587 (STARTTLS). For Gmail or Fastmail, create an app password instead of using your account password. Set `digest = true` to avoid a mail per item.

To check a channel without waiting for news, send it a test message:

```sh
gen notify-test --channel phone
```

## Web routes

| Route | Description |
|---|---|
| `/` | Latest items from all enabled sources. |
| `/engines` | Engine list with tier, C# support and recent activity. |
| `/engine/{id}` | Items for one engine. |
| `/sources` | Every source with its last fetch time and errors. Admins can switch sources on and off, add, remove and restore them here. |
| `/login`, `/logout` | Admin login with `server.admin_token`. |
| `/feed.xml` | Combined Atom feed. |
| `/api/items` | JSON API for items. |
| `/health` | Liveness check that returns `ok`. |

## Managing sources from the web

With `admin_token` set under `[server]`, the Sources page shows a **Manage sources** link. Log in with the token to manage sources without editing `sources.toml` or restarting. All changes are stored in the database and take effect right away; `sources.toml` is never written, so it can stay mounted read-only.

### Switching sources on and off

Each source (on `/sources` and on every engine page) gets an **Enable** / **Disable** button.

- The choice is stored in the database as an override of `enabled` in `sources.toml`, so it survives restarts and leaves the config file untouched. Sources marked `enabled = false` in the config can be switched on the same way.
- It takes effect immediately: a disabled source stops polling, and a re-enabled one polls right away. Items it already fetched stay visible.
- Only sources of enabled engines and outlets are listed; switch a whole engine on with `enabled = true` in `sources.toml`.

### Adding, removing and restoring sources

- **Add a source** (at the top of `/sources`, or the **Add source** link next to an engine or outlet) takes the same settings as a `[[engine.source]]` entry: a feed, GitHub repository, Hacker News search, scraped HTML page or the Unity releases API, plus an optional category, interval and keywords. Input is checked first (URLs, `owner/repo`, CSS selectors), and a source that already exists is refused. A new source is polled straight away; its first fetch is stored without sending notifications.
- **Remove** (next to Enable / Disable) asks for confirmation, then stops polling the source and **deletes its stored items**, fetch state and on/off override. Items elsewhere that were duplicates of its items are shown in their place.
- A removed source that comes from `sources.toml` is kept as a tombstone in the database and listed under **Removed from sources.toml**, where **Restore** brings it back (it then fetches its items again). A source added in the web UI is simply deleted.
- `gen check-config` validates only the config files; it doesn't include changes made in the web UI.

### Admin login

- The login sets an `HttpOnly`, `SameSite=Strict` cookie holding a hash of the token (also `Secure` when `base_url` is `https://`). Use a long random token, e.g. `openssl rand -hex 32`. Without a token, source management is off.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo fmt
```

Layout of `src/`:

| Module | Responsibility |
|---|---|
| `main.rs` | CLI (`serve`, `fetch`, `check-config`, `notify-test`) and wiring. |
| `config.rs` | `gen.toml` / `sources.toml` schema, validation and `${ENV}` expansion. |
| `model.rs` | Shared types: `ItemKind`, `RawItem`, `NewItem`, `Item`, `SourceState`. |
| `fetch/` | HTTP client and one fetcher per source kind: `feed`, `github`, `unity`, `hn`, `html`. |
| `classify.rs` | Item kind detection, engine tagging by alias and story keys for deduplication. |
| `store.rs` | SQLite storage: items, source state, web UI source edits and queries for the web UI. |
| `registry.rs` | The live set of sources: `sources.toml` plus web UI edits, published to the web UI, pipeline and scheduler. |
| `pipeline.rs` | Polls a source: fetch, classify, dedupe and store. |
| `notify/` | Rule matching and delivery: `discord`, `ntfy`, `email`. |
| `web/` | axum router: HTML pages (minijinja templates), Atom feed, JSON API. |

Runtime assets are in `templates/` (minijinja) and `static/` (CSS and other static files). Deployment files are in `deploy/`, `Dockerfile` and `docker-compose.yml`.
