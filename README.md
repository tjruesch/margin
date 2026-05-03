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
