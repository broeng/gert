# CLAUDE.md

## Project

**Gert** is a fast, keyboard-driven native desktop client for
[Gerrit Code Review](https://www.gerritcodereview.com/), written in Rust on
[GPUI](https://gpui.rs/) (the UI framework behind the Zed editor). It is an open
source project for the wider Gerrit community, licensed **GPL-3.0**, hosted at
<https://github.com/broeng/gert>.

Why it exists, in priority order when trade-offs collide:

1. **Speed and responsiveness.** Instant start from a local cache, no spinner for
   data we have already seen, never block the UI thread.
2. **Keyboard-driven flow.** Every action is reachable from the keyboard.
3. **Better diff and review UX** than the web UI (PolyGerrit).
4. **Desktop integration.** Notifications, local git and similar features come later.

The name lives in one place: `APP_NAME` in `crates/app/src/lib.rs`, used for
the config and cache dirs, the window title and Wayland app id, the keyring
service name and the HTTP user agent. Never hardcode "gert" elsewhere.

## Collaboration model

Claude writes most of the code and the user reviews it. So:

- Keep changes reviewable: one logical change per commit, no drive-by refactors.
- Explain non-obvious decisions in the commit body, not in chat.
- Claude **may** do these without asking: add dependencies (justify them in the
  commit message), bump the pinned GPUI / gpui-component revs and fix the
  breakage, and commit finished, tested work on a branch.
- Claude **must ask** before running any write operation (comment, vote, submit,
  reviewer change, ...) against a real Gerrit server. Writes may only go to the
  local dev Gerrit (`dev/README.md`).

## Principles

- **No telemetry, ever.** The app makes no network calls except to the
  Gerrit servers the user configured. That also rules out crash uploaders,
  update checks, and fetching fonts or avatars from third parties. Desktop
  notifications only talk to the local notification daemon.

## Platforms

- **Linux:** primary target. It must work and look decent on any desktop or WM;
  no GNOME- or KDE-specific integration. The window draws its own
  decorations (title bar and window controls) where the compositor wants
  client-side ones.
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
  - **SSO/OAuth servers:** the login page opens the system browser at the
    server's `settings/#HTTPCredentials` page; the user generates a password
    or token and pastes it back into the app. No embedded webview (GPUI has
    none), no browser-cookie scraping.
- Credentials live in the **OS keyring** (`keyring` crate: Secret Service on
  Linux, Keychain on macOS), one entry per `user@url`. Never write secrets to the config file, logs, or
  the cache DB. Debug builds also read `GERT_DEV_PASSWORD` for a server on
  127.0.0.1 / localhost, for headless testing against the dev Gerrit.
- Every Gerrit JSON response starts with the XSSI guard `)]}'\n`; strip it
  before parsing.
- The server ignores the diff `context` parameter and always sends the whole
  file, so context folding happens locally.

## Technology stack

| Concern | Choice |
|---|---|
| Language | Rust, latest stable (no `rust-toolchain.toml` pin); `rust-version` in `Cargo.toml` follows the devShell's stable |
| UI | GPUI, obtained **only** through `gpui-kit`'s re-exports |
| Widgets | `gpui-kit` from [gpui-component](https://github.com/longbridge/gpui-component), git dependency pinned to a commit |
| Theming | our own themes in `ui::theme` (CWC, Solarized), each with a light and dark variant picked by the system appearance; the theme is chosen in the settings page |
| Gerrit wire types | hand-written tolerant serde structs in `gerrit-api`, checked against the vendored OpenAPI spec |
| HTTP | our own async client over an `HttpClient` trait; the production impl in `app` adapts `gpui-pre-reqwest-client` (Zed's reqwest client, which manages its own tokio runtime) |
| Storage | SQLite via `rusqlite` (bundled), called from GPUI's background executor |
| Secrets | `keyring` |
| Desktop notifications | `notify-rust` in `app` only, pure-Rust zbus backend on Linux (no libdbus) |
| Config | TOML at `<config_dir>/gert/config.toml`, read with `toml` and written with `toml_edit` so the user's comments and formatting survive; cache DB in `<cache_dir>/gert/` (via `directories`) |
| Syntax highlighting | `syntect` with `two-face`'s syntax set, versions and features equal to gpui-component's workspace |
| File filters | `globset`, gitignore-style |
| Local checkouts | the `git` CLI at runtime (spawned, read-only plumbing commands), diffed in Rust with `imara-diff` (histogram) |
| Errors | `thiserror` in library crates, `anyhow` only in `app` |
| Logging | `log` facade (as Zed/GPUI use it), `env_logger` in `app` |

### GPUI pinning

- gpui-component (crate `gpui-kit`) is pinned by **git commit**. It pulls
  GPUI from crates.io as Zed's `gpui-pre-*` snapshot crates, pinned with an
  exact `=x.y.z` requirement in its workspace `Cargo.toml`.
- **Never depend on `gpui` / `gpui-pre` directly.** Use `gpui_kit::*`, which
  re-exports GPUI, plus `gpui_kit::component` and `gpui_kit::assets`. A
  second, differently-versioned GPUI would compile but be type-incompatible.
- The only allowed direct `gpui-pre-*` dependency is `gpui-pre-reqwest-client`
  in `app`, and its version **must equal** the `=x.y.z` that gpui-component's
  workspace `Cargo.toml` uses at our pinned commit. Bump both together, and
  `syntect` / `two-face` with them (see the comments in `Cargo.toml`).
- Do **not** git-pin `zed-industries/zed`. That would give a GPUI that
  gpui-component's widgets can't use.
- GPUI's API is unstable and largely undocumented. When unsure, read the source
  at the pinned versions (`~/.cargo/registry/src/*/gpui-pre-*`,
  `~/.cargo/git/checkouts/gpui-component-*`, especially its `examples/`)
  rather than guessing from memory; APIs change between snapshots.

### Wire types and the OpenAPI spec

- We do **not** use a generated Gerrit SDK. We tried
  [gerrit-sdk-rust](https://github.com/davido/gerrit-sdk-rust) and dropped it:
  its closed enums failed whole pages on unknown values, everything was
  `Option`/`i32`, and it pulled in blocking reqwest + native-tls.
- `gerrit-api` has hand-written serde **wire structs** (`wire` module) for
  the endpoints we use, and nothing else. They must be **tolerant**: unknown
  fields are ignored, enums keep unknown values (`Unknown(String)`), and
  optional server fields are `Option` or `#[serde(default)]`.
- Wire types are private to `gerrit-api`. They are converted into `model`
  types at the crate boundary (`convert` module).
- The Gerrit OpenAPI spec is **vendored** at `spec/rest-api-openapi.json` (see
  `spec/README.md` for where it came from). It is the reference for field names and
  types, the input for `cargo xtask gen-fixtures`, and a test checks that every
  wire struct field exists in the corresponding spec schema.

## Architecture

Cargo workspace:

```
crates/
  gerrit-api/   async REST client: auth, XSSI stripping, HttpClient trait,
                typed change query, read endpoints (changes, files, diffs,
                comments, related, preferences, actions) and writes (drafts,
                reviews, change actions, reviewers, topic/hashtags). Private
                tolerant wire structs, returns `model` types. A fake client
                behind the `test-support` feature.
  local-git/    reads file lists and diffs from a local checkout of a
                project through the `git` CLI, computed to match Gerrit's
                diffs (line splitting, whitespace modes, renames, intraline
                edits); finds the user's unpushed commits and maps lines
                between commits for porting comments. Read-only, never
                fetches; blocking, so callers use a background executor.
  model/        domain types: non-Option where semantically required, real
                timestamps, newtypes for ids (ChangeNumber, AccountId, ...).
                No I/O.
  store/        rusqlite cache + schema migrations: servers, query results,
                change details, file lists, diffs, related changes,
                preferences, comments, local drafts, the notification
                ledger, pending events and inbox, and local changes
                (`local_changes`, `local_patch_sets`, `pending_reviews`,
                `consolidation_leases`, `local_post_claims`).
  sync/         polling and cache refresh (server -> store -> UI
                notifications), and the write paths: drafts (saved to the
                store first, then sent and retried), reviews, change actions,
                reviewers, topic and hashtags; notification event detection,
                rules and matching (`sync::notify`); local changes:
                reconciling scans (`sync::local`), the local search subset
                and consolidation (`Syncer::consolidate`, porting through
                the `sync::PortSource` trait).
  ui/           GPUI views on gpui-component: the shell and page navigation,
                login, dashboard/search list, change page, diff page and diff
                view, reply dialog, action menu, file filters, settings,
                themes, project checkouts (`ui::checkouts`), the local
                diff source (`ui::local_source`) and local changes
                (`ui::local_scan`, `ui::consolidation`, ...).
                `examples/review.rs` runs
                the whole UI against a fake server; `examples/diff_viewer.rs`
                shows the diff view alone.
  gert/         (directory `crates/app`) binary: wiring, config, keyring,
                HttpClient impl over gpui-pre-reqwest-client.
dev/            local dev Gerrit: start, seed (`nix run .#dev-gerrit`)
spec/           vendored Gerrit OpenAPI spec
tests/fixtures/ schema-generated JSON fixtures
xtask/          fixture generation (`cargo xtask gen-fixtures`)
```

Dependency direction: `app → ui → sync → store, gerrit-api → model` and
`ui → local-git → model` (`ui` also uses `store` and `gerrit-api` types
directly). `model`, `gerrit-api`, `store`, `sync` and `local-git` must not
depend on GPUI, so they are testable without a UI. (`sync` exposes plain async/channels; `ui` adapts that to GPUI entities.)
`sync` reaches git only through `PortSource` (implemented in `ui`);
`local-git` is only a dev-dependency of `sync`, for its end-to-end test.

### Data flow and caching

- **Cache first:** the UI renders from `store`, never directly from a network
  response. `sync` fetches, writes to the store, and notifies the UI. That is
  what gives instant start and offline reading.
- **Refresh:** smart polling. The active and visible queries are polled on an
  interval, more often while the window is focused (`sync::poll`). Prefer
  cheap incremental queries (compare `updated`, `-age:`). The refresh source
  stays behind an interface so push sources could be added later, but don't
  build them now.
- **Drafts are store-first.** The UI's draft outbox (`ui::outbox`) writes a
  draft to the local database, shows it as pending at once, then sends it;
  failures are recorded on the draft and shown in its card, and pending
  drafts are retried. Votes, replies and change actions on Gerrit changes
  need a connection. A general offline outbox for those (replay with
  conflicts shown to the user: patch set moved on, thread resolved remotely)
  is not built yet, so don't make choices that would prevent it (keep write
  state in `store`, not only in view state). Its first piece exists: local
  reviews (see "Local changes") are stored first and sent later.
- **Several servers, one active at a time.** Config has `[[servers]]` (slug
  id, name, url, username, color, nested queries and bookmarks); the slug,
  not the URL, identifies a server in config, the cache (`servers.slug`) and
  navigation. Every cached row carries `server_id` and `delete_server`
  cascades. The `ui::servers` registry holds a session, draft outbox,
  syntax cache and connection state per server; each page belongs to one
  server and history entries carry it. Inactive servers are polled slowly
  for `attention:self` counts shown in the switcher (`Ctrl+K`, `Ctrl+1..9`).
  There is no merged all-servers view. Removing a server purges its cache
  rows and keyring secret.
- **Local checkouts.** A server's `[[servers.projects]]` entries (`name`,
  `checkout`; edited on the settings page's Projects tab, or via the gear
  next to the project on the change page) point at local git checkouts.
  When a checkout has the exact commits, a diff missing from the store and
  an uncached file list are computed from it (`ui::local_source`), so they
  show at once and offline; anything else (merge commits, rebased patch
  set comparisons, `/COMMIT_MSG` unless a local version is involved,
  missing commits, git errors, a resolve slower than 5 s) falls through to
  Gerrit. Order: store, then checkout,
  then Gerrit; Gerrit's file list still replaces a local one. Local results
  are **never written to the store** and carry a "local" marker in the UI.
  Git is only ever read (no fetch, no locks, no repo writes).

### Local changes

The user's own unpushed commits, reviewed before they are on Gerrit and
sent there automatically once pushed (the app never pushes).

- **Discovery.** Per project: `local_changes = true` and `local_approval`
  in `[[servers.projects]]` (project dialog; not for anonymous servers).
  `ui::local_scan` takes commits on named local branches and on no
  remote-tracking branch, with a `Change-Id` trailer and the account's or
  checkout's author email. `Syncer::reconcile_local` makes one local
  change per Change-Id (project, target branch) and a local patch set
  ("local 1", ...) per SHA seen; an unknown SHA of an open Gerrit change
  is a local patch set of that change. A change gone from the branches
  goes with its data, unless it has unsent data: then it stays until a
  lookup settles it.
- **Synthetic numbers.** Local changes and patch sets get numbers from
  reserved ranges (`ChangeNumber::LOCAL_BASE`, `PatchSetNumber::LOCAL_BASE`)
  so pages, drafts and history work on them. They never reach a server:
  `gerrit-api` refuses them (`Error::LocalNumber`; the fake panics), `sync`
  refuses local SHAs on revision calls and drops them from batches, and
  browser links and `g` refuse them. Details are synthesised from git and
  the store (`ui::change_detail`; labels borrowed from a cached change of
  the project and branch) and never stored as change details.
- **Local reviews are store-first.** A comment on a local version is a
  draft on its local numbers. The reply dialog ("Local review", Save)
  writes a `pending_reviews` row whose message is a sealed draft; quick
  votes join the newest pending review or start one without a message.
  It can be edited or cancelled until sent.
- **The "ordinary draft" rule.** Only ordinary drafts (`Draft::is_ordinary`,
  `drafts::ordinary()` in SQL: not on local numbers, not sealed, not
  reserved for a consolidation post, not held) take part in upload, retry,
  `publish_review`, merging server drafts and the counts. Every new draft
  path must keep to it. A reply to a Gerrit thread is an ordinary draft
  on the thread's own patch set, also while a local version is shown.
- **Consolidation** (`Syncer::consolidate`, run per server by
  `ui::consolidation::Consolidator` while online with a known account: on
  the poll, after a scan that changed rows, after a saved review). A
  batched lookup (`project~branch~Change-Id` when the branch is known,
  else the Change-Id); on an exact SHA match the drafts and reviews move
  onto the Gerrit patch set and go out as one post per review (comments
  inline, the message as a patch-set comment, Gerrit's default notify,
  attention set rules off), tagged `<app>-local-<change>-r<review>-<nonce>`
  so a lost answer is recovered, not resent. A version amended before
  upload is **ported** through the checkout (`ui::local_port`): comments
  on unchanged lines move, anything else is **held** with a reason. A
  change found merged or abandoned has everything held. Held items are on
  the Gerrit change's held card (publish or drop). With nothing left the
  row is a `Consolidated` tombstone (its local number redirects) until its
  commits have been off the branches for a week. One run per server (a
  store lease) and a claim per post; only the account that wrote an item
  sends it.
- **Approval rules.** An approval is a label's highest positive value.
  Votes are cast only on the current patch set, all of a review together
  or none. An exact SHA match casts them. A review ported from an amended
  version follows the **strict rule** unless its votes are **purely
  local**: nothing is cast, approvals are held drop-only and other votes
  held to publish by hand. Purely local (`sync::purely_local`) means that
  when the votes were given the change was not on Gerrit, or Gerrit was
  unreachable: the review was not on a local patch set of a Gerrit change,
  and its votes were saved offline or before a lookup that found no such
  change started (local clock only; no time window). A review is judged by
  its latest vote, not per label. Purely local votes then follow the
  stricter of the `local_approval` saved with the review and the
  project's now: `follow` casts them (also when comments were held),
  `exact-or-manual` holds them all to publish by hand, `exact` (the
  default) holds approvals drop-only and the others to publish. For the
  user: votes saved online and pushed before any lookup ran (a push right
  after saving) are held by the strict rule, since Gerrit could not
  confirm the change was not there yet; saving starts a lookup at once,
  so this takes a very quick push.
- **In the UI:** closed and held rows on the dashboard open the Gerrit
  change; free search lists local results first, from a strict subset of
  the query language; consolidation shows quiet page hints, not OS
  notifications.
- **Known gaps:** the change page's `l` and `p s` chords don't fire while
  an inline diff has focus; only versions seen while the app runs are
  known; closed-on-arrival is tested against the fake server only.

### Notifications

- Events are **derived from sync diffs**: whenever `Syncer::refresh_change`
  saves a fresh change detail on a watched server, `sync::diff_change`
  compares it with the cached one and queues `model::Event`s (votes,
  comments, attention set, reviewers, patch sets, merged/abandoned, ready to
  submit, merge conflict, mentions) in the store's `pending_events`; the
  `notified_events` ledger dedupes them. The first complete watch pass on a
  server (`servers.watched_since`) and changes never seen before are
  baselines and never notify.
- `sync::notify` holds the rule model, the matcher (scope, labels, exact
  vote values, actors, own actions, per-server), `watch_query`, quiet hours
  and the planner that groups events into OS notifications. `ui::watching`
  runs one watch loop per signed-in (not anonymous) server, including the
  active one; `Shell::found_events` delivers through the `ui::Notifier`
  trait (`app::notifier::DesktopNotifier` over `notify-rust`), stores every
  matched event in the inbox and acknowledges in one transaction.
- Config is `[notifications]` plus `[[notifications.rules]]`, edited on the
  settings page's Notifications tab. With no rules, the defaults apply
  (attention set added, negative Code-Review/Verified votes and comments on
  my changes). Notifications work only while the app runs; no tray or
  background mode.
- The inbox (bell in the title bar, `Ctrl-Shift-N`) is the one cross-server
  page and keeps every matched event, also those the OS notification
  skipped (quiet hours, focused window, no daemon).
- Known gaps: `only_unresolved` and `replies_to_me` are stored but not
  evaluated, `exclude_bots` has no effect (accounts carry no tags), and
  click-to-open works on Linux only.

### Diffs

- Diffs come from the **server diff API**
  (`/changes/{id}/revisions/{rev}/files/{file}/diff` → `DiffInfo`, including
  intraline edits), so comment anchors match what other reviewers see, or
  from a configured local checkout (`local-git`), computed to be the same
  diff (see "Local checkouts").
- Side-by-side and unified views, with local context folding (the server
  sends whole files).
- Long lines wrap when Gerrit's `line_wrapping` diff preference is on;
  otherwise the code text scrolls sideways, all panes together, with the
  gutters fixed.
- **Syntax highlighting** with syntect: each side is highlighted as a whole on
  the background executor, the diff is drawn plain until the result arrives,
  and the colors come from the app theme at paint time. It follows Gerrit's
  `syntax_highlighting` diff preference.
- The diff view is our own widget. Since we are GPL-3.0 we *may* adapt code from
  Zed's GPL crates (editor etc.) and Apache-2.0 code from gpui-component, but
  keep attribution and don't pull in Zed's editor crate wholesale.

### UI

- **PolyGerrit-like page navigation:** dashboard/search → change → file diff,
  plus login and settings pages, with back/forward history. Not a multi-pane
  or tabbed workspace.
- **Keybindings mirror the Gerrit web UI** (`j`/`k`, `n`/`p`, `a` reply, `r`
  mark reviewed, `/` search, `?` help, ...) so Gerrit users feel at home.
  They are GPUI actions and keymaps, not ad-hoc key handlers, so they can be
  made configurable later. Each binding is an entry with its help text (in
  `ui::actions` or a module's own `actions`), and `ui::actions` gathers them,
  so the `?` help is built from the same table as the keymap.
- The settings page edits `config.toml` (saved queries, startup page, file
  filters, theme, review options, project checkouts and local changes); the
  user may also edit it by hand.
- Never do I/O or heavy work on the main thread. Use background tasks, and
  update entities when the work finishes.

## Status

Built so far: login (anonymous, HTTP password, SSO via pasted token);
dashboard of saved queries and free search, cached and polled; change page
(header, submit requirements, votes, reviewers, commit message, relation chain
with a side panel, file list, change log); diff page with side-by-side and
unified diffs, context folding, syntax highlighting and inline comment
threads; drafts, replies and votes (reply dialog and quick votes), reviewed
marks and the attention set; change actions (submit, rebase,
abandon/restore, WIP/ready, private, reviewers/CC, topic, hashtags) via an
action menu; file filters (hide, dim, highlight by glob); a settings page;
opening a change from a web UI link (`g`, `gert <link>`, or a
`gert://<host><path>` link from a browser, matched against all configured
servers); multiple servers with a switcher; desktop
notifications with configurable rules and an in-app inbox; a tabbed settings
page; per-project local checkouts that serve file lists and diffs when they
have the commits; local changes (unpushed commits reviewed locally, sent
automatically once pushed, with porting and held items).

Not built yet: Checks/CI results, further local git integration (fetching,
checking out changes), query autocompletion, a configurable keymap and command palette,
the general offline outbox (votes, replies and actions on Gerrit changes),
inline change edits, Gerrit's own
dashboard sections, and packaging beyond cargo and Nix.

## Distribution and Nix

- Distribution for now: build from source (`cargo install --path crates/app`)
  and the Nix flake. No Flatpak, AppImage, or distro packages yet.
- `flake.nix` at the repo root provides:
  - `packages.default` / `apps.default`, so `nix build` and `nix run` work. It is
    built with **crane** (dependencies are cached separately, and git
    dependencies are vendored from `Cargo.lock`). It installs the desktop
    entry and icon from `dist/`; the entry registers the `gert://` URL scheme.
  - `apps.dev-gerrit` / `apps.dev-gerrit-seed`: the local dev Gerrit
    (`dev/README.md`).
  - `devShells.default`: the toolchain, rust-analyzer, and every native
    library GPUI needs (vulkan-loader, wayland, libxkbcommon, xorg libxcb/X11,
    fontconfig, freetype, ...), with `LD_LIBRARY_PATH` set so plain
    `cargo run` works.
  - The Rust toolchain comes from **rust-overlay** (latest stable), not nixpkgs.
- It must work on **NixOS and on Nix running on other distros**. On non-NixOS
  hosts, Nix-built GPU apps can't find the host's Vulkan/GL drivers; the
  workaround (nixGL / nix-gl-host) belongs in the README (not written yet),
  rather than hacking the flake for one distro.
- When adding a native dependency, add it to both the package and the devShell.
  Runtime tools are different: the package's wrapper puts `gitMinimal` at the
  end of `PATH` (the user's own git wins), and the devShell relies on the
  host's git.

## Testing

- `model` / `gerrit-api`: unit tests against **schema-generated JSON fixtures**
  in `tests/fixtures/`, produced by `cargo xtask gen-fixtures` from the OpenAPI
  vendored spec in `spec/`. Fixtures should exercise every field and
  edge cases (missing optional fields, unknown enum values, empty maps), and
  must include the XSSI prefix where a real response would have it.
- `gerrit-api` takes an `HttpClient` trait object, so tests use the fake client
  (`test-support` feature). No network in `cargo test`.
- `store`: tests against in-memory SQLite, including migrations from every
  previous schema version.
- `sync`: tests over the fake client and an in-memory store.
- `local-git`: tests against temporary repos built with the git CLI (the
  helpers are exported behind its `test-support` feature for `ui`'s tests).
- `ui`: GPUI's `TestAppContext` for view logic. The slowest tests (whole
  pages and shell flows, temporary git repos) are ignored unless the
  `slow-tests` feature is on: per-commit runs (`cargo test -p ui`) skip them,
  and every batch of work ends with
  `cargo test --workspace --features ui/slow-tests`, which runs them all.
  Mark a new test that takes more than about 0.3 s with
  `#[cfg_attr(not(feature = "slow-tests"), ignore = "slow: --features slow-tests")]`.
- **Opt-in, ignored by default:** `tests/dev_gerrit.rs` in `gerrit-api`,
  `sync` and `local-git` (which compares its diffs with Gerrit's; see its
  header for the setup; `sync`'s includes local changes end to end with a
  real clone and `git push`), and a test in `app`, run against the dev Gerrit
  (`GERT_TEST_GERRIT=http://127.0.0.1:8080 cargo test -- --ignored`, refusing
  anything but localhost); `gerrit-api/tests/live.rs` reads a public server
  with `GERT_LIVE_TESTS=1`. Run these per crate and test target: a
  workspace-wide `--ignored` also runs `ui`'s slow tests.

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
cargo run -p gert                # run the client
cargo run -p ui --example review # the whole UI against a fake server
cargo test -p ui                 # per commit: ui without the slow tests
cargo test --workspace --features ui/slow-tests  # end of a batch: all tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo xtask gen-fixtures         # regenerate tests/fixtures from the spec
nix develop                      # dev shell with toolchain + native libs
nix run                          # build and run via the flake
nix run .#dev-gerrit -- --seed   # local dev Gerrit on 127.0.0.1:8080
```

## Open questions

- App ID (reverse-DNS, e.g. `io.github.broeng.Gert`).
