# Local dev Gerrit

M3 and later add writes (drafts, replies, votes, ...). They may only ever go to
a Gerrit you run locally. This directory has everything to run one from Nix,
without Docker or Podman.

- `dev-gerrit.sh`: initializes a throwaway site and runs Gerrit in the
  foreground.
- `seed.sh`: fills a running dev Gerrit with users, a project and changes.

## Start

```
nix run .#dev-gerrit                 # http://127.0.0.1:8080, Ctrl-C stops it
nix run .#dev-gerrit -- --seed       # same, and seed on the first start
nix run .#dev-gerrit -- --port 8081 --site /some/dir --reset
```

The first start initializes the site (a few seconds), then waits until the REST
API answers and prints the URL. Needs only Nix; the package is the `gerrit`
from nixpkgs (3.14.x at the time of writing) with a JDK 21. It is plain Java,
so nothing special is required on non-NixOS hosts.

What the site looks like: auth type `DEVELOPMENT_BECOME_ANY_ACCOUNT` (the web UI
at the URL has a "become" login), listening on 127.0.0.1 only, SSH daemon off
(we are REST only), Lucene index, no outgoing mail, no bundled plugins.
`git push` over HTTP works with an HTTP password.

Environment variables (flags win): `GERT_DEV_GERRIT_SITE` (default
`$XDG_RUNTIME_DIR/gert-dev-gerrit`, else `$TMPDIR` or `/tmp`) and
`GERT_DEV_GERRIT_PORT` (default 8080). `--reset` deletes the site first.
The site is not meant to survive reboots when it lives under `$XDG_RUNTIME_DIR`.

## Seed

With the server running, in another terminal:

```
nix run .#dev-gerrit-seed            # honours the same env vars / --port, --site
```

It takes under a minute and is safe to repeat: on a seeded site it says so and
does nothing (use `--reset` for a fresh one).

How the first account is made: the dev login page has a "New Account" form; the
first account registered through it becomes an administrator. The seeder posts
that form once, uses the session cookie and XSRF token to set the user name
`admin` and an HTTP password, and from then on only uses basic auth on `/a/`.
Other users are created with `PUT /a/accounts/{name}`.

### Users

Password of every user is `<user>-dev-password` (also in
`<site>/credentials.txt`; nothing secret, nothing in the repo):

| user | role in the data |
|---|---|
| `admin` | Administrators; approves and submits |
| `alice` | owner of the big change, has drafts (the default test user) |
| `bob` | reviewer, owner of the change chain |
| `carol` | reviewer (votes -1), owner of the WIP and the submittable change |
| `ci-bot` | votes `Verified` |

### Data

Project `demo/scheduler` (branch `main`, labels `Code-Review` and `Verified`,
all registered users may vote on both). Changes on a fresh site:

| # | state | notes |
|---|---|---|
| 1 | merged | "Add scheduler skeleton" (alice), topic `skeleton` |
| 2 | merged | "Fix typo in README" (carol) |
| 3 | open | "Retry failed jobs with exponential backoff" (alice): 3 patch sets, adds, modifies, rename (`util.rs` to `helpers.rs`), delete, a binary file and a 400 line file with two distant edits, topic `retry-logic`, hashtags `backend` and `perf`, votes Code-Review +1 (bob), -1 (carol), Verified +1 (ci-bot), published comments (commit message, file-level, line, range, PARENT side, replies, resolved and unresolved, on all three patch sets) and four drafts by alice |
| 4, 5, 6 | open | a related chain by bob (topic `cleanup`); #5 has a -1 from alice and a draft by bob |
| 7 | abandoned | by alice, with a -1 from bob |
| 8 | open, WIP | carol, hashtag `docs` |
| 9 | open, submittable | carol, Code-Review +2 and Verified +1 |

## Use the app against it

Log in with URL `http://127.0.0.1:8080`, user `alice` and password
`alice-dev-password`.

## Integration tests

```
GERT_TEST_GERRIT=http://127.0.0.1:8080 \
  cargo test -p gerrit-api --test dev_gerrit -- --ignored --nocapture
```

`GERT_TEST_USER` / `GERT_TEST_PASSWORD` default to alice. The tests
read only, so they can be repeated. Without the variable they are not run
(`cargo test --workspace` never touches the network).

## Facts learned from the server

- Gerrit 3.14.4 accepts `context=ALL` on the file diff endpoint, but ignores
  the `context` parameter altogether: the diff always holds the whole file and
  never a `skip` chunk. The client has to collapse unchanged runs itself.
- Voting on a change you were CC'd on makes you a reviewer.

## Troubleshooting

- `gerrit init` needs `ssh-keygen`; the Nix app puts openssh on the PATH. When
  running the scripts without Nix, provide `java` (21), `ssh-keygen`, `git`,
  `curl` and `jq`, and set `GERT_GERRIT_WAR` to a `gerrit.war`.
- Logs: `<site>/logs/error_log`, `<site>/init.log`, `<site>/daemon.log`.
