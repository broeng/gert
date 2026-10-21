#!/usr/bin/env bash
# Seeds a running dev Gerrit (see dev-gerrit.sh) with users, a project and a
# set of changes that exercise the read paths (and later the write paths) of
# the client. Uses only curl, git and jq, and only talks to $DEV_URL
# (127.0.0.1). Run through `dev-gerrit.sh seed`, which exports DEV_URL,
# DEV_SITE and DEV_PORT.
#
# Account bootstrap: with auth.type = DEVELOPMENT_BECOME_ANY_ACCOUNT the
# login page has a "New Account" button. The first account registered that
# way lands in the Administrators group. We register it with one scripted
# POST, then (using the session cookie and the XSRF token) give it the user
# name "admin" and an HTTP password. Everything else uses basic auth on /a/.
set -euo pipefail

: "${DEV_URL:?run through dev-gerrit.sh seed}"
: "${DEV_SITE:?run through dev-gerrit.sh seed}"
: "${DEV_PORT:?run through dev-gerrit.sh seed}"
case $DEV_URL in
  http://127.0.0.1:* | http://localhost:*) ;;
  *) echo "refusing to seed anything but a local dev Gerrit ($DEV_URL)" >&2; exit 1 ;;
esac

PROJECT=demo/scheduler
PROJECT_ENC=demo%2Fscheduler
CREDS="$DEV_SITE/credentials.txt"
WORK="$DEV_SITE/seed-work"
export GIT_TERMINAL_PROMPT=0

pw() { printf '%s-dev-password' "$1"; }

# api USER METHOD PATH [JSON]: call /a/PATH, print the body without XSSI.
api() {
  local user=$1 method=$2 path=$3 data=${4:-} out code
  local args=(-sS -u "$user:$(pw "$user")" -X "$method" -w '\n%{http_code}')
  if [[ -n $data ]]; then
    args+=(-H 'Content-Type: application/json' -d "$data")
  fi
  out=$(curl "${args[@]}" "$DEV_URL/a$path")
  code=${out##*$'\n'}
  out=${out%$'\n'*}
  if [[ $code != 2* ]]; then
    echo "ERROR: $method $path -> HTTP $code: $out" >&2
    return 1
  fi
  printf '%s\n' "$out" | sed '1{/^)\]}'"'"'$/d}'
}

if [[ -f $CREDS ]] || curl -fs -o /dev/null -u "admin:$(pw admin)" "$DEV_URL/a/projects/$PROJECT_ENC"; then
  echo "Already seeded ($CREDS exists or $PROJECT is there)."
  echo "Use dev-gerrit.sh --reset for a fresh site."
  exit 0
fi

echo "== Accounts"
jar=$(mktemp)
trap 'rm -f "$jar"' EXIT
curl -fsS -c "$jar" -o /dev/null -d action=create_account "$DEV_URL/login/%23%2F"
curl -fsS -b "$jar" -c "$jar" -o /dev/null "$DEV_URL/" # sets XSRF_TOKEN
xsrf=$(awk '$6 == "XSRF_TOKEN" { print $7 }' "$jar")
cookie_put() {
  curl -fsS -b "$jar" -X PUT -H "X-Gerrit-Auth: $xsrf" -H 'Content-Type: application/json' \
    -d "$2" -o /dev/null "$DEV_URL/accounts/self/$1"
}
cookie_put username '{"username":"admin"}'
cookie_put name '{"name":"Admin"}'
cookie_put password.http "{\"http_password\":\"$(pw admin)\"}"
api admin GET /accounts/self/groups | jq -e 'any(.[]; .name == "Administrators")' >/dev/null ||
  { echo "first account is not an administrator" >&2; exit 1; }

mkuser() { # username Full Name
  api admin PUT "/accounts/$1" "$(jq -n --arg n "$2" --arg e "$1@example.com" --arg p "$(pw "$1")" \
    '{name:$n,email:$e,http_password:$p}')" >/dev/null
}
mkuser alice "Alice Anderson"
mkuser bob "Bob Brown"
mkuser carol "Carol Clark"
mkuser ci-bot "CI Bot"
api admin PUT /accounts/admin/emails/admin@example.com '{"preferred":true,"no_confirmation":true}' >/dev/null || true

echo "== Project $PROJECT"
api admin PUT "/projects/$PROJECT_ENC" \
  '{"branches":["main"],"create_empty_commit":true,"description":"Demo project for the Gert dev Gerrit"}' >/dev/null
api admin PUT "/projects/$PROJECT_ENC/HEAD" '{"ref":"refs/heads/main"}' >/dev/null
api admin PUT "/projects/$PROJECT_ENC/labels/Verified" \
  '{"values":{"-1":"Fails","0":"No score","+1":"Verified"},"default_value":0,"function":"NoOp","commit_message":"Add Verified"}' >/dev/null
api admin POST "/projects/$PROJECT_ENC/access" '{"add":{"refs/heads/*":{"permissions":{
  "label-Verified":{"label":"Verified","rules":{"global:Registered-Users":{"action":"ALLOW","min":-1,"max":1}}},
  "label-Code-Review":{"label":"Code-Review","rules":{"global:Registered-Users":{"action":"ALLOW","min":-2,"max":2}}}
}}}}' >/dev/null

echo "== Git work tree"
rm -rf "$WORK"
git init -q -b main "$WORK"
G() { git -C "$WORK" -c credential.helper= "$@"; }
remote() { printf 'http://%s:%s@127.0.0.1:%s/a/%s' "$1" "$(pw "$1")" "$DEV_PORT" "$PROJECT"; }
G fetch -q "$(remote admin)" refs/heads/main:refs/remotes/origin/main
G checkout -q -B work origin/main
MAIN_BASE=$(G rev-parse origin/main)

change_id() { printf 'I%s' "$(printf 'gert-dev-%s' "$1" | sha1sum | cut -c1-40)"; }
fullname() { api admin GET "/accounts/$1" | jq -r .name; }

# commit USER KEY SUBJECT [BODY]: commit the working tree as USER.
commit() {
  local user=$1 key=$2 subject=$3 body=${4:-} msg
  msg=$subject
  [[ -n $body ]] && msg+=$'\n\n'$body
  msg+=$'\n\n'"Change-Id: $(change_id "$key")"
  G add -A
  printf '%s\n' "$msg" | G -c user.name="$(fullname "$user")" -c user.email="$user@example.com" commit -q -F -
}
amend() { # USER
  G add -A
  G -c user.name="$(fullname "$1")" -c user.email="$1@example.com" commit -q --amend --no-edit
}
# push USER OPTIONS: push HEAD for review.
push() {
  local out
  if ! out=$(G push "$(remote "$1")" "HEAD:refs/for/main%$2" 2>&1); then
    echo "push failed: $out" >&2
    return 1
  fi
}
num() { api admin GET "/changes/$PROJECT_ENC~main~$(change_id "$1")" | jq -r ._number; }
lineof() { grep -n -m1 -- "$2" "$WORK/$1" | cut -d: -f1; }

# review USER CHANGE REV JSON: post a review (labels, message, comments).
review() { api "$1" POST "/changes/$2/revisions/$3/review" "$4" >/dev/null; }
# draft USER CHANGE REV JSON
draft() { api "$1" PUT "/changes/$2/revisions/$3/drafts" "$4" >/dev/null; }
# comment_id USER CHANGE MESSAGE-PREFIX: id of a published comment.
comment_id() {
  api "$1" GET "/changes/$2/comments" |
    jq -r --arg p "$3" '[.[][] | select(.message | startswith($p))][0].id'
}

write_base() {
  mkdir -p "$WORK/src" "$WORK/assets" "$WORK/data"
  cat >"$WORK/Cargo.toml" <<'T'
[package]
name = "scheduler"
version = "0.1.0"
edition = "2021"
T
  cat >"$WORK/README.md" <<'T'
# Scheduler

A tiny job scheduler used to demo the Gert dev Gerrit.
Teh queue is first in, first out.
T
  cat >"$WORK/src/main.rs" <<'T'
mod queue;
mod util;

use std::time::Duration;

fn main() {
    let mut q = queue::Queue::new();
    q.push("first", Duration::from_secs(1));
    q.push("second", Duration::from_secs(2));
    while let Some(job) = q.pop() {
        println!("running {} ({})", job.name, util::describe(job.delay));
    }
}
T
  cat >"$WORK/src/queue.rs" <<'T'
//! A tiny job queue.

use std::collections::VecDeque;
use std::time::Duration;

/// A unit of work.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub delay: Duration,
}

/// FIFO queue of jobs.
#[derive(Default)]
pub struct Queue {
    jobs: VecDeque<Job>,
    next_id: u64,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, name: &str, delay: Duration) -> u64 {
        self.next_id += 1;
        self.jobs.push_back(Job {
            id: self.next_id,
            name: name.to_string(),
            delay,
        });
        self.next_id
    }

    pub fn pop(&mut self) -> Option<Job> {
        self.jobs.pop_front()
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }
}
T
  cat >"$WORK/src/util.rs" <<'T'
use std::time::Duration;

/// Human readable duration.
pub fn describe(d: Duration) -> String {
    if d.as_secs() >= 60 {
        format!("{} min", d.as_secs() / 60)
    } else {
        format!("{} s", d.as_secs())
    }
}

/// Clamp a delay to a sane maximum.
pub fn clamp(d: Duration) -> Duration {
    d.min(Duration::from_secs(3600))
}
T
  cat >"$WORK/src/old.rs" <<'T'
//! Legacy scheduler, superseded by queue.rs.

pub fn legacy_run() {
    println!("legacy");
}
T
  # Binary file: PNG signature plus NUL bytes, so git treats it as binary.
  { printf '\x89PNG\r\n\x1a\n\0\0\0\rIHDR'; head -c 256 /dev/zero | tr '\0' '\1'; } >"$WORK/assets/logo.png"
  seq 1 400 | sed 's/.*/line & of the sample data file: lorem ipsum dolor sit amet/' >"$WORK/data/big.txt"
}

echo "== Base commits (merged)"
write_base
commit alice base "Add scheduler skeleton" "A FIFO job queue with a tiny runner, helpers and sample data."
push alice "topic=skeleton,hashtag=initial,r=bob"
N_BASE=$(num base)
review bob "$N_BASE" 1 '{"message":"Looks fine as a start.","labels":{"Code-Review":1}}'
review admin "$N_BASE" 1 '{"message":"Approved.","labels":{"Code-Review":2}}'
api admin POST "/changes/$N_BASE/submit" '{}' >/dev/null
G fetch -q "$(remote admin)" refs/heads/main:refs/remotes/origin/main
G checkout -q -B work origin/main

sed -i 's/Teh queue/The queue/' "$WORK/README.md"
commit carol typo "Fix typo in README"
push carol "r=alice"
N_TYPO=$(num typo)
review alice "$N_TYPO" 1 '{"message":"Thanks!","labels":{"Code-Review":1}}'
review admin "$N_TYPO" 1 '{"labels":{"Code-Review":2}}'
api admin POST "/changes/$N_TYPO/submit" '{}' >/dev/null
G fetch -q "$(remote admin)" refs/heads/main:refs/remotes/origin/main
G checkout -q -B work origin/main
MAIN=$(G rev-parse HEAD)

echo "== Change A: three patch sets, many file kinds"
cat >>"$WORK/src/queue.rs" <<'T'

impl Queue {
    /// Put a job back at the end of the queue after a failure.
    pub fn requeue(&mut self, job: Job) {
        let delay = job.delay * 2;
        self.jobs.push_back(Job { delay, ..job });
    }
}
T
cat >"$WORK/src/retry.rs" <<'T'
//! Retry policy for failed jobs.

use std::time::Duration;

pub struct Retry {
    pub max_attempts: u32,
    pub base_delay: Duration,
}

impl Retry {
    pub fn delay_for(&self, attempt: u32) -> Duration {
        self.base_delay * 2u32.pow(attempt)
    }
}
T
G mv src/util.rs src/helpers.rs
sed -i 's|/// Human readable duration.|/// Human readable duration (seconds or minutes).|' "$WORK/src/helpers.rs"
G rm -q src/old.rs
sed -i 's/^mod util;/mod helpers;\nmod retry;/; s/util::describe/helpers::describe/' "$WORK/src/main.rs"
{ printf '\x89PNG\r\n\x1a\n\0\0\0\rIHDR'; head -c 300 /dev/zero | tr '\0' '\2'; } >"$WORK/assets/logo.png"
sed -i '20s/.*/line 20 of the sample data file: CHANGED in patch set 1/; 380s/.*/line 380 of the sample data file: CHANGED in patch set 1/' "$WORK/data/big.txt"
commit alice A "Retry failed jobs with exponential backoff" "Failed jobs go back to the queue with a doubled delay. The retry policy
lives in its own module, util.rs becomes helpers.rs, and the legacy runner
is gone. The logo is refreshed too."
push alice "topic=retry-logic,hashtag=backend,hashtag=perf,r=bob,cc=carol,cc=ci-bot"
N_A=$(num A)

L_REQ=$(lineof src/queue.rs 'let delay = job.delay \* 2;')
L_POP=$(lineof src/queue.rs 'pub fn pop')
review bob "$N_A" 1 "$(jq -n --argjson l "$L_REQ" --argjson p "$L_POP" '{
  message: "Some thoughts on the first patch set.",
  labels: {"Code-Review": -1},
  comments: {
    "/COMMIT_MSG": [{line: 1, message: "Should this mention the backoff factor?", unresolved: true}],
    "src/retry.rs": [
      {message: "Nothing here is wired up yet, please add a caller.", unresolved: true},
      {line: 11, message: "This overflows for large attempt counts, use checked_pow.", unresolved: true}
    ],
    "src/queue.rs": [
      {line: $l, range: {start_line: $l, start_character: 8, end_line: $l, end_character: 34},
       message: "Cap the doubled delay, otherwise it grows without bound.", unresolved: true},
      {line: $p, side: "PARENT", message: "Not a change from you, but pop() could return the id too.", unresolved: false}
    ],
    "assets/logo.png": [{message: "Is the new logo final?", unresolved: false}]
  }}')"
review alice "$N_A" 1 "$(jq -n --argjson p "$L_POP" --arg id "$(comment_id alice "$N_A" 'Not a change from you')" '{
  comments: {"src/queue.rs": [{line: $p, side: "PARENT", in_reply_to: $id, message: "Good idea, will do that separately.", unresolved: false}]}}')"

# Drafts by alice on patch set 1 (they stay drafts; the thread is Bob's).
draft alice "$N_A" 1 "$(jq -n --arg id "$(comment_id alice "$N_A" 'Cap the doubled delay')" \
  '{path: "src/queue.rs", line: 0, in_reply_to: $id, message: "Done in the next patch set.", unresolved: false}' |
  jq --argjson l "$L_REQ" '.line = $l')"

echo "== Change A: patch set 2"
python_cap='s|let delay = job.delay \* 2;|let delay = crate::helpers::clamp(job.delay * 2);|'
sed -i "$python_cap" "$WORK/src/queue.rs"
sed -i 's|self.base_delay \* 2u32.pow(attempt)|self.base_delay * 2u32.saturating_pow(attempt)|' "$WORK/src/retry.rs"
cat >>"$WORK/src/retry.rs" <<'T'

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_each_attempt() {
        let r = Retry { max_attempts: 3, base_delay: Duration::from_secs(1) };
        assert_eq!(r.delay_for(2), Duration::from_secs(4));
    }
}
T
sed -i '20s/.*/line 20 of the sample data file: CHANGED in patch set 2/' "$WORK/data/big.txt"
amend alice
push alice "m=Cap_the_delay_and_add_a_test"
L_CLAMP=$(lineof src/queue.rs 'clamp(job.delay')
review carol "$N_A" 2 "$(jq -n --argjson l "$L_CLAMP" '{
  message: "Looks closer. One nit on the clamp.",
  labels: {"Code-Review": 1},
  comments: {"src/queue.rs": [{line: $l, message: "Nit: name the one hour limit as a constant.", unresolved: true}]}}')"
review ci-bot "$N_A" 2 '{"message":"Build failed: cargo test (1 failure)","labels":{"Verified":-1}}'

echo "== Change A: patch set 3"
sed -i '380s/.*/line 380 of the sample data file: CHANGED in patch set 3/' "$WORK/data/big.txt"
sed -i 's|^pub fn clamp(d: Duration) -> Duration {|pub fn clamp(d: Duration) -> Duration {\n    // One hour is the longest delay we ever want.|' "$WORK/src/helpers.rs"
amend alice
push alice "m=Comment_the_limit"
review bob "$N_A" 3 '{"message":"Better, thanks.","labels":{"Code-Review":1}}'
review carol "$N_A" 3 "$(jq -n '{message: "Still not happy about the unbounded attempts.", labels: {"Code-Review": -1}}')"
review ci-bot "$N_A" 3 '{"message":"Build succeeded","labels":{"Verified":1}}'
# alice resolves carol's nit with a published reply, and keeps a draft per patch set.
review alice "$N_A" 3 "$(jq -n --arg id "$(comment_id alice "$N_A" 'Nit: name the one hour')" '{
  comments: {"src/queue.rs": [{line: 1, in_reply_to: $id, message: "Done", unresolved: false}]}}' |
  jq --argjson l "$(lineof src/queue.rs 'clamp(job.delay')" '.comments["src/queue.rs"][0].line = $l')"
draft alice "$N_A" 3 "$(jq -n '{path: "src/retry.rs", line: 11, message: "Should max_attempts be checked here?", unresolved: true}')"
draft alice "$N_A" 3 "$(jq -n '{path: "src/helpers.rs", message: "File-level draft: rename describe to format_delay?", unresolved: true}')"
draft alice "$N_A" 3 "$(jq -n '{path: "/COMMIT_MSG", line: 3, message: "Draft on the commit message, range included.", unresolved: false, range: {start_line: 3, start_character: 0, end_line: 3, end_character: 10}}')"

echo "== Change chain B (related changes), bob"
G checkout -q -B chain "$MAIN"
sed -i 's|println!("running|println!("starting|' "$WORK/src/main.rs"
commit bob B1 "Say starting instead of running" "The job has not run yet when this is printed."
cat >"$WORK/src/config.rs" <<'T'
//! Runtime configuration.

pub struct Config {
    pub workers: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self { workers: 1 }
    }
}
T
commit bob B2 "Add a Config type" "Only the worker count for now."
cat >>"$WORK/src/queue.rs" <<'T'

impl Queue {
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}
T
commit bob B3 "Add Queue::is_empty" "Clippy asks for it next to len()."
push bob "topic=cleanup,hashtag=backend,r=alice,r=carol"
N_B1=$(num B1)
N_B2=$(num B2)
N_B3=$(num B3)
review alice "$N_B2" 1 '{"message":"Why not read the worker count from the environment?","labels":{"Code-Review":-1},"comments":{"src/config.rs":[{"line":9,"message":"Hardcoded for now?","unresolved":true}]}}'
draft bob "$N_B2" 1 '{"path":"src/config.rs","line":9,"message":"Yes, a follow-up will add parsing.","unresolved":false}'

echo "== Abandoned change C, alice"
G checkout -q -B abandoned "$MAIN"
echo "pub const DEBUG: bool = true;" >"$WORK/src/debug.rs"
commit alice C "Add a debug flag" "Superseded by the config work."
push alice "topic=cleanup,r=bob"
N_C=$(num C)
review bob "$N_C" 1 '{"message":"Please use the config type instead.","labels":{"Code-Review":-1}}'
api alice POST "/changes/$N_C/abandon" '{"message":"Superseded by the Config change."}' >/dev/null

echo "== WIP change D, carol"
G checkout -q -B wip "$MAIN"
cat >>"$WORK/README.md" <<'T'

## Usage

Run `cargo run`. Work in progress: document the retry policy here.
T
commit carol D "Document usage in the README" "Not ready yet, the retry section is missing."
push carol "wip,hashtag=docs"
N_D=$(num D)

echo "== Submittable change E, carol"
G checkout -q -B submittable "$MAIN"
sed -i 's|Duration::from_secs(2)|Duration::from_secs(5)|' "$WORK/src/main.rs"
commit carol E "Use a longer second delay in the demo" "Makes the output easier to follow."
push carol "r=alice,r=bob,r=ci-bot"
N_E=$(num E)
review admin "$N_E" 1 '{"message":"LGTM","labels":{"Code-Review":2}}'
review ci-bot "$N_E" 1 '{"labels":{"Verified":1}}'
review bob "$N_E" 1 '{"labels":{"Code-Review":1},"comments":{"src/main.rs":[{"line":9,"message":"Nice.","unresolved":false}]}}'

echo "== Credentials"
{
  for u in admin alice bob carol ci-bot; do
    printf '%s %s\n' "$u" "$(pw "$u")"
  done
} >"$CREDS"
chmod 600 "$CREDS"

cat <<MSG

Seeded $PROJECT on $DEV_URL
  users (name password): see $CREDS (password is <user>-dev-password)
  changes:
    #$N_BASE  merged   Add scheduler skeleton (alice)
    #$N_TYPO  merged   Fix typo in README (carol)
    #$N_A  open     3 patch sets, renames/deletes/binary/large file (alice)
    #$N_B1 #$N_B2 #$N_B3  open  a related chain (bob)
    #$N_C  abandoned
    #$N_D  WIP
    #$N_E  open, submittable

Run the client tests with:
  GERT_TEST_GERRIT=$DEV_URL cargo test -p gerrit-api -- --ignored
MSG
