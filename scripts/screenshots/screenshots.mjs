// Retakes the README screenshots in docs/screenshots/.
//
// Builds GEN, writes a throwaway gen.toml and database in a temp folder, adds a few catalog
// engines and an outlet, fetches once, serves on a free port, logs in as admin and captures
// each view in light and dark mode. The repo's data/ is never touched, and the temp folder is
// removed and the server stopped even when a step fails. See README.md in this folder.

import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { setTimeout as sleep } from "node:timers/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const OUT = join(REPO, "docs/screenshots");
const GEN = join(REPO, "target/debug/gen");
const ADMIN_TOKEN = "screenshot-admin-token";
const VIEWPORT = { width: 1280, height: 900 };

// Added on top of the seeded Unity, Unreal Engine and Godot, for a mix of engines and kinds.
const EXTRA_OWNERS = ["bevy", "defold", "gamefromscratch"];

// The views to capture. `admin` views are captured logged in.
const VIEWS = [
  { name: "timeline", path: "/", admin: false },
  { name: "engine", path: "/engine/godot", admin: false },
  { name: "sources", path: "/sources", admin: true },
  { name: "notifications", path: "/notifications", admin: true },
];

function log(msg) {
  console.log(`[screenshots] ${msg}`);
}

function freePort() {
  return new Promise((ok, fail) => {
    const srv = createServer();
    srv.once("error", fail);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => ok(port));
    });
  });
}

// The example channels' secrets are ${...} references to the placeholders below, which the
// page shows as written. They are set only for `gen serve` (an unset reference shows the
// channel as failing) and lead nowhere: a closed local port and reserved example.com hosts.
const PLACEHOLDER_SECRETS = {
  GEN_SCREENSHOT_WEBHOOK: "http://127.0.0.1:9/never-sent",
  GEN_SCREENSHOT_NTFY_TOPIC: "never-sent",
  GEN_SCREENSHOT_SMTP_USER: "never-sent",
  GEN_SCREENSHOT_SMTP_PASSWORD: "never-sent",
};

function configToml(dir, port) {
  return `[server]
bind = "127.0.0.1:${port}"
base_url = "http://localhost:${port}"
templates_dir = ${JSON.stringify(join(REPO, "templates"))}
static_dir = ${JSON.stringify(join(REPO, "static"))}
admin_token = "${ADMIN_TOKEN}"

[database]
path = ${JSON.stringify(join(dir, "gen.db"))}

[polling]
default_interval_minutes = 60
concurrency = 8

[[notify.channel]]
name = "discord-releases"
kind = "discord"
webhook_url = "\${GEN_SCREENSHOT_WEBHOOK}"
  [notify.channel.rules]
  engines = ["godot", "bevy", "defold"]
  kinds = ["release"]

[[notify.channel]]
name = "phone"
kind = "ntfy"
server = "https://ntfy.example.com"
topic = "\${GEN_SCREENSHOT_NTFY_TOPIC}"
health_alerts = true
  [notify.channel.rules]
  kinds = ["release", "news"]
  keywords = ["LTS", "stable"]

[[notify.channel]]
name = "weekly-digest"
kind = "email"
smtp_host = "smtp.example.com"
smtp_port = 587
username = "\${GEN_SCREENSHOT_SMTP_USER}"
password = "\${GEN_SCREENSHOT_SMTP_PASSWORD}"
from = "GEN <gen@example.com>"
to = ["you@example.com"]
digest = true
  [notify.channel.rules]
  search = "godot shader"
`;
}

// Runs a gen subcommand and returns its output (fetch lists every item, so it isn't printed).
function gen(config, ...args) {
  return execFileSync(GEN, ["--config", config, ...args], {
    cwd: REPO,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
  }).trim();
}

function startServer(config) {
  const server = spawn(GEN, ["--config", config, "serve"], {
    cwd: REPO,
    stdio: ["ignore", "ignore", "inherit"],
    env: { ...process.env, ...PLACEHOLDER_SECRETS, RUST_LOG: process.env.RUST_LOG ?? "warn" },
  });
  server.exited = new Promise((ok) => server.once("exit", ok));
  return server;
}

async function waitForHealth(base, server) {
  const deadline = Date.now() + 30_000;
  let exited = false;
  server.exited.then(() => (exited = true));
  while (Date.now() < deadline && !exited) {
    try {
      const res = await fetch(`${base}/health`);
      if (res.ok && (await res.text()).trim() === "ok") return;
    } catch {
      // not listening yet
    }
    await sleep(200);
  }
  throw new Error(exited ? "gen serve exited before it was healthy" : "gen serve did not get healthy within 30s");
}

// Logs in through the /login form and checks the admin-only Notifications link shows.
async function logIn(context, base) {
  const page = await context.newPage();
  await page.goto(`${base}/login`);
  await page.fill('input[name="token"]', ADMIN_TOKEN);
  await Promise.all([page.waitForNavigation(), page.click('button[type="submit"]')]);
  if ((await page.locator('.site-nav a[href="/notifications"]').count()) === 0) {
    throw new Error("admin login failed: no Notifications link after logging in");
  }
  await page.close();
}

// Captures one view at the viewport size (not the full page), once fonts have loaded.
async function capture(context, base, view, theme) {
  const page = await context.newPage();
  const res = await page.goto(`${base}${view.path}`, { waitUntil: "networkidle" });
  if (!res.ok()) throw new Error(`${view.path} answered ${res.status()}`);
  if (new URL(page.url()).pathname === "/login") throw new Error(`${view.path} redirected to the login form`);
  await page.evaluate(() => document.fonts.ready);
  const file = join(OUT, `${view.name}-${theme}.png`);
  await page.screenshot({ path: file });
  await page.close();
  log(`captured ${file}`);
}

async function main() {
  log("building gen");
  execFileSync("cargo", ["build", "--locked", "--quiet"], { cwd: REPO, stdio: "inherit" });

  const dir = mkdtempSync(join(tmpdir(), "gen-screenshots-"));
  let server;
  try {
    const port = await freePort();
    const config = join(dir, "gen.toml");
    writeFileSync(config, configToml(dir, port));
    log(`temp folder ${dir}, port ${port}`);

    for (const id of EXTRA_OWNERS) {
      gen(config, "add", id);
      log(`added ${id}`);
    }
    log(`fetched: ${gen(config, "fetch").split("\n").pop()}`);

    const base = `http://127.0.0.1:${port}`;
    server = startServer(config);
    await waitForHealth(base, server);
    log(`serving on ${base}`);

    const browser = await chromium.launch();
    try {
      mkdirSync(OUT, { recursive: true });
      for (const theme of ["light", "dark"]) {
        const options = { viewport: VIEWPORT, deviceScaleFactor: 1, colorScheme: theme };
        const visitor = await browser.newContext(options);
        const admin = await browser.newContext(options);
        await logIn(admin, base);
        for (const view of VIEWS) {
          await capture(view.admin ? admin : visitor, base, view, theme);
        }
      }
    } finally {
      await browser.close();
    }
  } finally {
    if (server && server.exitCode === null) {
      server.kill("SIGTERM");
      await server.exited;
    }
    rmSync(dir, { recursive: true, force: true });
    log("removed temp folder");
  }
}

main().catch((err) => {
  console.error(`[screenshots] failed: ${err.stack || err}`);
  process.exit(1);
});
