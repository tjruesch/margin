# Margin

A lightweight, GitHub-flavored Markdown editor for macOS — Tauri + React + CodeMirror 6.

## Develop

```sh
bun install
bun run tauri dev
```

## Build

```sh
bun run tauri build
```

Produces a `.dmg` and `.app` in `src-tauri/target/release/bundle/`.

## OAuth client IDs

Margin's connector platform supports Google and Microsoft (calendar, with email / chat etc. to follow). Each provider's client ID is read from an environment variable at build time — no secrets land in the source tree.

To enable a provider, register a desktop OAuth app in its developer console and export the client ID before `cargo build` / `bun run tauri build` / `bun run tauri dev`:

### Google

1. [Google Cloud Console → APIs & Services → Credentials](https://console.cloud.google.com/apis/credentials)
2. Create an **OAuth client ID** of type **Desktop**.
3. Under "Authorized redirect URIs," add `http://127.0.0.1:8765` through `http://127.0.0.1:8784` (Margin's loopback range).
4. Enable the **Google Calendar API** in APIs & Services → Library.
5. Copy the client ID (the public string ending in `.apps.googleusercontent.com`).
6. Export it:
   ```sh
   export MARGIN_GOOGLE_CLIENT_ID="123456789-abc.apps.googleusercontent.com"
   ```

### Microsoft

1. [Azure App Registrations](https://portal.azure.com/#blade/Microsoft_AAD_RegisteredApps/ApplicationsListBlade) → **New registration**.
2. Account type: **Personal Microsoft accounts only** (or include work/school as needed).
3. Redirect URI type: **Public client/native (mobile & desktop)**, value `http://127.0.0.1:8765` (add the rest of the range too: `8766`..`8784`).
4. API permissions → Microsoft Graph → **Calendars.Read**, **User.Read**, **offline_access**.
5. Copy the **Application (client) ID** from Overview.
6. Export it:
   ```sh
   export MARGIN_MICROSOFT_CLIENT_ID="00000000-0000-0000-0000-000000000000"
   ```

Without these env vars set, the corresponding provider simply doesn't appear in Settings → Connectors → Add. PKCE means no client secret is required.

## Shortcuts

| Action     | Key            |
| ---------- | -------------- |
| Open       | ⌘O             |
| Save       | ⌘S             |
| Save As    | ⌘⇧S            |
| Edit mode  | ⌘E             |
| Preview    | ⌘P             |

## Stack

- Tauri 2 (Rust backend, system WebKit on macOS)
- React 19 + Vite
- CodeMirror 6 with the markdown language pack
- markdown-it (GFM) + highlight.js, themed by `github-markdown-css`
