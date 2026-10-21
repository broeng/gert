#!/usr/bin/env bash
# Local development Gerrit: a throwaway site that listens on 127.0.0.1 only.
#
#   dev-gerrit.sh [run] [--port N] [--site DIR] [--reset] [--seed]
#   dev-gerrit.sh seed  [--port N] [--site DIR]        (against a running one)
#
# Environment (all optional when run through `nix run .#dev-gerrit`):
#   GERT_GERRIT_WAR       path to gerrit.war (set by the flake)
#   GERT_DEV_GERRIT_SITE  site directory
#   GERT_DEV_GERRIT_PORT  HTTP port
#
# See dev/README.md. This script never talks to anything but 127.0.0.1.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cmd=run
if [[ "${1:-}" == run || "${1:-}" == seed ]]; then
  cmd=$1
  shift
fi

port=${GERT_DEV_GERRIT_PORT:-8080}
site=${GERT_DEV_GERRIT_SITE:-${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/gert-dev-gerrit}
reset=0
seed_after=0
while (($#)); do
  case $1 in
    --port) port=$2; shift ;;
    --site) site=$2; shift ;;
    --reset) reset=1 ;;
    --seed) seed_after=1 ;;
    -h | --help) sed -n '2,13p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

export DEV_PORT=$port DEV_SITE=$site
export DEV_URL="http://127.0.0.1:$port"

# Wait until the REST API answers.
wait_ready() {
  local i
  for ((i = 0; i < 240; i++)); do
    if curl -fs -o /dev/null "$DEV_URL/config/server/version"; then
      return 0
    fi
    if [[ -n "${gerrit_pid:-}" ]] && ! kill -0 "$gerrit_pid" 2>/dev/null; then
      echo "Gerrit exited during startup, see $site/logs/error_log" >&2
      return 1
    fi
    sleep 1
  done
  echo "timed out waiting for Gerrit at $DEV_URL" >&2
  return 1
}

if [[ $cmd == seed ]]; then
  wait_ready
  exec bash "$here/seed.sh"
fi

war=${GERT_GERRIT_WAR:-}
if [[ -z $war || ! -f $war ]]; then
  echo "GERT_GERRIT_WAR must point at gerrit.war (use nix run .#dev-gerrit)" >&2
  exit 1
fi

if curl -fs -o /dev/null "$DEV_URL/config/server/version"; then
  echo "something already answers on $DEV_URL; pick another --port" >&2
  exit 1
fi

if ((reset)); then
  rm -rf "$site"
fi

if [[ ! -f $site/etc/gerrit.config ]]; then
  echo "Initializing a new site in $site ..."
  mkdir -p "$site"
  # --skip-plugins: no bundled plugins (replication, gitiles, ...), nothing
  # the REST work needs. ssh-keygen (openssh) must be on PATH for the host keys.
  java -jar "$war" init --batch --no-auto-start --skip-plugins -d "$site" \
    >"$site/init.log" 2>&1 || { cat "$site/init.log" >&2; exit 1; }
  cfg() { git config -f "$site/etc/gerrit.config" "$@"; }
  cfg gerrit.canonicalWebUrl "$DEV_URL/"
  cfg auth.type DEVELOPMENT_BECOME_ANY_ACCOUNT
  # Let `git push` over HTTP use the HTTP password with basic auth.
  cfg auth.gitBasicAuthPolicy HTTP
  cfg httpd.listenUrl "http://127.0.0.1:$port/"
  # REST only: no SSH daemon at all.
  cfg sshd.listenAddress off
  cfg index.type lucene
  cfg sendemail.enable false
  cfg plugins.allowRemoteAdmin false
  cfg download.scheme http
  cfg container.heapLimit 1g
  touch "$site/.gert-site"
else
  # Keep the URL and port consistent when reusing a site with another port.
  git config -f "$site/etc/gerrit.config" gerrit.canonicalWebUrl "$DEV_URL/"
  git config -f "$site/etc/gerrit.config" httpd.listenUrl "http://127.0.0.1:$port/"
fi

echo "Starting Gerrit (log: $site/logs/error_log) ..."
java -jar "$war" daemon -d "$site" --console-log >"$site/daemon.log" 2>&1 &
gerrit_pid=$!
stop() {
  trap - INT TERM EXIT
  echo
  echo "Stopping Gerrit ..."
  kill "$gerrit_pid" 2>/dev/null || true
  wait "$gerrit_pid" 2>/dev/null || true
  # Nothing may be left running.
}
trap stop INT TERM EXIT

wait_ready

if ((seed_after)); then
  bash "$here/seed.sh"
fi

cat <<MSG

Gerrit $(curl -fs "$DEV_URL/config/server/version" | sed -e 1d -e 's/"//g') is ready.
  URL:   $DEV_URL
  Site:  $site
  Auth:  DEVELOPMENT_BECOME_ANY_ACCOUNT (the web UI has a "become" login)
  Seed:  $( [[ -f $site/credentials.txt ]] && echo "done, credentials in $site/credentials.txt" || echo "not run yet: dev-gerrit.sh seed (or nix run .#dev-gerrit-seed)")

Press Ctrl-C to stop.
MSG

wait "$gerrit_pid"
