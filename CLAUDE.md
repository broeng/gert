# CLAUDE.md

## Project

**Gert** is a fast, keyboard-driven native desktop client for
[Gerrit Code Review](https://www.gerritcodereview.com/), written in Rust on
[GPUI](https://gpui.rs/) (the UI framework behind the Zed editor). It is an open
source project for the wider Gerrit community, licensed **GPL-3.0**.

Why it exists, in priority order when trade-offs collide:

1. **Speed and responsiveness.** Instant start from a local cache, no spinner for
   data we have already seen, never block the UI thread.
2. **Keyboard-driven flow.** Every action is reachable from the keyboard.
3. **Better diff and review UX** than the web UI (PolyGerrit).
4. **Desktop integration.** Notifications, local git and similar features come later.

Keep the name in one place (an `APP_NAME` / `APP_ID` constant in
`app`, used for config dirs, the window title, and the keyring service name), and
never hardcode "gert" elsewhere.

## Collaboration model

Claude writes most of the code and the user reviews it. So:

- Keep changes reviewable: one logical change per commit, no drive-by refactors.
- Explain non-obvious decisions in the commit body, not in chat.
- Claude **may** do these without asking: add dependencies (justify them in the
  commit message), bump the pinned GPUI / gpui-component revs and fix the
  breakage, and commit finished, tested work on a branch.
- Claude **must ask** before running any write operation (comment, vote, submit,
  reviewer change, ...) against a real Gerrit server. Writes may only go to a
  local dev Gerrit.

## Principles

- **No telemetry, ever.** The app makes no network calls except to the
  Gerrit servers the user configured. That also rules out crash uploaders,
  update checks, and fetching fonts or avatars from third parties.

## Platforms

- **Linux:** primary target. It must work and look decent on any desktop or WM;
  no GNOME- or KDE-specific integration.
- **macOS:** secondary. The user can test it, so don't break it.
- **Windows:** bonus only. Don't spend effort on it, but don't gratuitously
  break it either (use `directories` for paths, not hand-built `~/.config`).

## Gerrit compatibility

- Target **Gerrit 3.x** REST API only. No 2.x support and no feature detection
  for ancient servers. Fields that newer servers add must be optional in our
  domain model when older 3.x servers omit them.
- REST only. SSH (stream-events etc.) is out of scope for now.
- Authentication:
  - **Anonymous**, read-only, for public servers.
  - **HTTP password / token** via Basic auth on the `/a/` prefix.
  - **SSO/OAuth servers:** open the system browser at the server's HTTP
    credentials settings page; the user generates a password or token and
    pastes it back into the app. No embedded webview (GPUI has none), no
    browser-cookie scraping.
- Credentials live in the **OS keyring** (`keyring` crate: Secret Service on
  Linux, Keychain on macOS). Never write secrets to the config file, logs, or
  the cache DB.
- Every Gerrit JSON response starts with the XSSI guard `)]}'\n`; strip it
  before parsing.

## Technology stack

| Concern | Choice |
|---|---|
| Language | Rust, latest stable (no `rust-toolchain.toml` pin) |
| UI | GPUI, git dependency pinned to a specific `zed-industries/zed` rev |
| Widgets/theming | [gpui-component](https://github.com/longbridge/gpui-component), Zed-like dark/light themes following the system setting |
| Gerrit models | `gerrit_client` from [gerrit-sdk-rust](https://github.com/davido/gerrit-sdk-rust), **models only**, git tag pin |
| HTTP | our own async client over an `HttpClient` trait; production impl is `reqwest` on a small dedicated tokio runtime (same pattern as Zed's `reqwest_client`) |
| Storage | SQLite via `rusqlite` (bundled), called from GPUI's background executor |
| Secrets | `keyring` |
| Config | TOML at `<config_dir>/<app>/config.toml`; cache DB in `<cache_dir>/<app>/` (via `directories`) |
| Errors | `thiserror` in library crates, `anyhow` only in `app` |
| Logging | `log` facade (as Zed/GPUI use it) |

### GPUI pinning

- GPUI and gpui-component are pinned by git rev. **gpui-component itself pins a
  zed rev; our `gpui` rev must match it exactly**, or Cargo builds two
  incompatible copies of GPUI. When bumping, bump gpui-component first and take
  its zed rev.
- GPUI's API is unstable and largely undocumented. When unsure, read the source
  at the pinned rev (in `~/.cargo/git/checkouts/`) and gpui-component's examples
  rather than guessing from memory; APIs change between revs.

### gerrit-sdk-rust

- It is a **generated prototype** (OpenAPI → openapi-generator) pinned to a
  Gerrit 3.15.0-SNAPSHOT spec. Its API functions are blocking reqwest, and some
  endpoints are untyped (e.g. `GET /changes/` returns `Vec<serde_json::Value>`).
- We use **only its `models`** for deserializing responses. We do **not** call
  its `apis::*` functions.
- `gerrit_client` types **never leak out of `gerrit-api`**. They are converted
  into our own `model` types at the crate boundary, so the SDK stays swappable.
- Known cost: depending on it compiles blocking reqwest and native-tls even
  though we don't use them. If that becomes a problem, propose an upstream
  change making the transport optional rather than forking.

## Architecture

Cargo workspace:

```
crates/
  gerrit-api/   async REST client: auth, XSSI stripping, HttpClient trait,
                typed endpoints (incl. typed change query). Deserializes with
                gerrit_client models, returns `model` types.
  model/        domain types: non-Option where semantically required, real
                timestamps, newtypes for ids (ChangeNumber, AccountId, ...).
                No I/O.
  store/        rusqlite cache + schema migrations. Outbox tables for offline
                writes come later.
  sync/         polling and cache refresh: server -> store -> UI notifications.
  ui/           GPUI views, built on gpui-component.
  app/          binary: wiring, config, keyring, reqwest/tokio HttpClient impl.
tests/fixtures/ schema-generated JSON fixtures
xtask/          fixture generation and other dev tooling (`cargo xtask ...`)
```

Dependency direction: `app → ui → sync → store, gerrit-api → model`.
`model`, `gerrit-api`, `store` and `sync` must not depend on GPUI, so they are
testable without a UI. (`sync` exposes plain async/channels; `ui` adapts that to
GPUI entities.)

### Data flow and caching

- **Cache first:** the UI renders from `store`, never directly from a network
  response. `sync` fetches, writes to the store, and notifies the UI. That is
  what gives instant start and offline reading.
- **Refresh:** smart polling. Poll the active and visible queries on an
  interval, more often while the window is focused and backing off when it
  isn't. Prefer cheap incremental queries (compare `updated`, `-age:`).
  Keep the refresh source behind an interface so push sources could be added
  later, but don't build them now.
- **Offline writes (designed for, not built yet):** comments, votes and other
  writes will go through a local outbox and be replayed when back online, with
  conflicts shown to the user (patchset moved on, thread resolved remotely).
  Until then, writes need a connection, but don't make choices that would
  prevent an outbox later (e.g. keep draft state in `store`, not only in view
  state).
- **Single server** for now. Store the server identity in the schema and in
  keys anyway, so multi-server support is a later feature rather than a migration
  nightmare.

### Diffs

- Diffs come from the **server diff API**
  (`/changes/{id}/revisions/{rev}/files/{file}/diff` → `DiffInfo`, including
  intraline edits), so comment anchors match what other reviewers see.
- Plain text first; syntax highlighting comes later (probably tree-sitter).
- The diff view is our own widget. Since we are GPL-3.0 we *may* adapt code from
  Zed's GPL crates (editor etc.), but keep attribution and don't pull in Zed's
  editor crate wholesale.

### UI

- **PolyGerrit-like page navigation:** query/dashboard → change → file diff,
  with back/forward history. Not a multi-pane or tabbed workspace.
- **Keybindings mirror the Gerrit web UI** (`j`/`k`, `n`/`p`, `a` reply, `r`
  mark reviewed, `/` search, `?` help, ...) so Gerrit users feel at home.
  Define them through GPUI actions and keymaps, not ad-hoc key handlers, so
  they can be made configurable later.
- Never do I/O or heavy work on the main thread. Use background tasks, and
  update entities when the work finishes.

## Roadmap

- **M1: Search and saved queries.** Proves the stack end to end.
  - Server config + login (anonymous, or HTTP password stored in the keyring).
  - "Dashboard" = a list of user-configured saved queries, seeded with sensible
    defaults (e.g. `is:open owner:self`, `is:open reviewer:self -owner:self`).
    The real Gerrit dashboard sections are not in M1.
  - Search box with arbitrary queries (plain text input; query-syntax
    autocompletion comes later); result list showing status, votes and size;
    full keyboard navigation. The attention set ("Your Turn") is just another
    saved query, not a special UI concept.
  - Results cached in SQLite, shown immediately on start, refreshed by polling.
  - Opening a change shows a placeholder page with change metadata only.
- **M2: Change view and diffs.** Change metadata, file list, patchset
  selection, side-by-side and unified diffs.
- **M3: Comments and voting.** Threads, drafts, replies, label votes. At the end
  of M3 the app is usable for review.
- **M4: Change actions.** Submit, rebase, abandon/restore, reviewers,
  topic/hashtags.
- **Later:** Checks/CI results, notifications, local git integration, inline
  change edits, offline write outbox, multi-server, syntax highlighting,
  configurable keymap.

## Distribution and Nix

- Distribution for now: build from source (`cargo install --path crates/app`)
  and the Nix flake. No Flatpak, AppImage, or distro packages yet.
- `flake.nix` at the repo root provides:
  - `packages.default` / `apps.default`, so `nix build` and `nix run` work. It is
    built with **crane** (dependencies are cached separately, and git
    dependencies are vendored from `Cargo.lock`).
  - `devShells.default`: the toolchain, rust-analyzer, and every native
    library GPUI needs (vulkan-loader, wayland, libxkbcommon, xorg libxcb/X11,
    fontconfig, freetype, openssl, ...), with `LD_LIBRARY_PATH` set so plain
    `cargo run` works.
  - The Rust toolchain comes from **rust-overlay** (latest stable), not nixpkgs.
- It must work on **NixOS and on Nix running on other distros**. On non-NixOS
  hosts, Nix-built GPU apps can't find the host's Vulkan/GL drivers; document
  the workaround (nixGL / nix-gl-host) in the README rather than hacking the
  flake for one distro.
- When adding a native dependency, add it to both the package and the devShell.

## Testing

- `model` / `gerrit-api`: unit tests against **schema-generated JSON fixtures**
  in `tests/fixtures/`, produced by `cargo xtask gen-fixtures` from the OpenAPI
  spec shipped with gerrit-sdk-rust. Fixtures should exercise every field and
  edge cases (missing optional fields, unknown enum values, empty maps), and
  must include the XSSI prefix where a real response would have it.
- `gerrit-api` takes an `HttpClient` trait object, so tests use a fake client.
  No network in `cargo test`.
- `store`: tests against in-memory SQLite, including migrations from every
  previous schema version.
- `ui`: GPUI's `TestAppContext` for view logic where practical.

## Conventions

- `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` must
  pass before every commit.
- No `unwrap()` / `expect()` in non-test code, except for a genuine invariant,
  which must be documented in the `expect` message.
- Typed errors (`thiserror`) in library crates; `anyhow` only in `app`.
- Commit messages are Gerrit/kernel style: an imperative subject of at most 72
  characters with a component prefix (`api: Add typed change query`), a blank
  line, then a body explaining *why*.

## Commands

```
cargo build                      # build everything
cargo run -p gert                 # run the client
cargo test --workspace           # all tests (no network)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo xtask gen-fixtures         # regenerate tests/fixtures from the spec
nix develop                      # dev shell with toolchain + native libs
nix run                          # build and run via the flake
```

## Open questions

- Hosting/forge for the project (GitHub vs a Gerrit instance vs other).
- App ID (reverse-DNS, tied to the hosting choice).
