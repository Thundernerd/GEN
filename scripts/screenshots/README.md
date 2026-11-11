# README screenshots

`screenshots.mjs` retakes the screenshots in `docs/screenshots/` that the main README shows. It uses [Playwright](https://playwright.dev) and Node.js 18 or newer. It isn't part of the Cargo build or CI.

```sh
cd scripts/screenshots
npm ci
npx playwright install chromium
npm run screenshots
```

The script:

1. builds GEN (`cargo build --locked`);
2. writes a throwaway `gen.toml` and database in a temp folder, with a free port, an admin token and three example notification channels;
3. adds Bevy, Defold and GameFromScratch to the seeded Unity, Unreal Engine and Godot, and runs `gen fetch` once;
4. starts `gen serve`, waits for `/health`, and logs in through `/login` for the admin views;
5. captures `/`, `/engine/godot`, `/sources` and `/notifications` at 1280×900, in light and dark mode, as `docs/screenshots/<view>-<light|dark>.png`;
6. stops the server and removes the temp folder, also when a step fails.

The repo's `data/` folder is never touched. Set `GITHUB_TOKEN` to have the GitHub sources use the API, as `gen` does (e.g. `GITHUB_TOKEN=$(gh auth token) npm run screenshots`).

The example channels' secrets are `${GEN_SCREENSHOT_*}` references, set only for the server to values that lead nowhere (a closed local port and reserved `example.com` hosts), so nothing is ever sent.

The items are fetched live, so every run gives different images. Retake them when the web UI changes, and check each image by eye before committing.
