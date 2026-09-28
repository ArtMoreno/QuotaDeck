#!/usr/bin/env bash
# Build, link, enable, and configure QuotaDeck in one step.
#
# Usage:
#   ./install.sh
#   ./install.sh --agent claude,codex
#   ./install.sh --watch-interval-seconds 300
#   ./install.sh --sidebar-layout stacked
#   ./install.sh --row-gap 0
#   ./install.sh --quota-percent used
#   ./install.sh --fields topic,model,context,5h,7d
#   ./install.sh --brand-colors off
#   ./install.sh --brand-glyphs unicode
#   ./install.sh --agent-order quota
#   ./install.sh --low-quota-alert 10
#   ./install.sh --http-proxy http://127.0.0.1:7890
#
# --agent installs only the agents you name (all, claude, codex, grok, agy,
# opencode, pi, omp, hermes). Anything you leave out gets no sidebar row, no statusLine
# entry and no hook file. The default is every supported agent.
#
# --sidebar-layout packed (default) joins cache/TTL and 5h/7d on one row.
# stacked puts provider, model, cache, TTL, context, 5h, and 7d on their own rows.
#
# --row-gap 1 (default) leaves one blank row between agent panes; 0 packs them
# flush. Herdr only accepts whole rows.
#
# --quota-percent remaining (default) shows how much quota is left; used shows
# how much has been consumed. The colour always follows what is left.
#
# --fields picks the quota fields the sidebar shows: all, none, or a
# comma-separated list of topic, model, cache, ttl, context, 5h, 7d. By default
# everything except prompt-derived topic is on; use all to opt in. The provider
# and the error token are always shown.
#
# --brand-colors on (default) tints provider and model with each agent's hue;
# off leaves them in the sidebar's own text colour. Severity colours stay.
#
# --brand-glyphs icon (default) uses Herdr Agent Icons Max, unicode uses marks
# any monospace font carries, and off shows provider names alone.
#
# --agent-order default (default) leaves Herdr's own agent panel ordering
# alone. quota asks Herdr to put the agent with the least quota left at the
# top; it replaces the panel's sort until it is set back to default.
#
# --low-quota-alert off (default) never notifies. A percentage notifies once,
# per provider, when its remaining quota falls to that number or below, and
# again only after it has recovered above it.
#
# --http-proxy sets an HTTP proxy for quota requests, or off for direct access.
# HTTPS requests use an HTTP CONNECT tunnel. Omit to keep the existing choice.
# Display options can also be changed later in the QuotaDeck settings pane.
#
# Every option is written to the plugin config directory before configure runs.
# Herdr executes a plugin action with a fixed command line in the server's own
# environment, so exported variables never reach it; the config directory is
# the only channel that does.
#
# The matching uninstall.sh first restores every configuration entry owned by
# this plugin and only then unlinks it from Herdr.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=scripts/herdr-action.sh
source "$ROOT/scripts/herdr-action.sh"
WATCH_INTERVAL_SECONDS=""
AGENTS=""
SIDEBAR_LAYOUT=""
ROW_GAP=""
QUOTA_PERCENT=""
FIELDS=""
BRAND_COLORS=""
BRAND_GLYPHS=""
AGENT_ORDER=""
LOW_QUOTA_ALERT=""
QUOTA_HTTP_PROXY=""

while (($# > 0)); do
  case "$1" in
    --watch-interval-seconds)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      WATCH_INTERVAL_SECONDS="$2"
      shift 2
      ;;
    --agent)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      AGENTS="$2"
      shift 2
      ;;
    --sidebar-layout)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      SIDEBAR_LAYOUT="$2"
      shift 2
      ;;
    --row-gap)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      ROW_GAP="$2"
      shift 2
      ;;
    --quota-percent)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      QUOTA_PERCENT="$2"
      shift 2
      ;;
    --fields)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      FIELDS="$2"
      shift 2
      ;;
    --brand-colors)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      BRAND_COLORS="$2"
      shift 2
      ;;
    --brand-glyphs)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      BRAND_GLYPHS="$2"
      shift 2
      ;;
    --agent-order)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      AGENT_ORDER="$2"
      shift 2
      ;;
    --low-quota-alert)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      LOW_QUOTA_ALERT="$2"
      shift 2
      ;;
    --http-proxy)
      (($# >= 2)) || { printf 'error: missing value for %s\n' "$1" >&2; exit 1; }
      QUOTA_HTTP_PROXY="$2"
      shift 2
      ;;
    -h|--help)
      sed -n '2,/^set -euo pipefail/{ /^set -euo pipefail/d; p; }' "$0"
      exit 0
      ;;
    *)
      printf 'error: unknown argument: %s\n' "$1" >&2
      exit 1
      ;;
  esac
done

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

command -v herdr >/dev/null 2>&1 || die "Herdr is not installed or not on PATH"
command -v cargo >/dev/null 2>&1 || die "Rust/Cargo is not installed or not on PATH"

case "$SIDEBAR_LAYOUT" in
  ""|packed|stacked) ;;
  *) die "sidebar-layout must be packed or stacked" ;;
esac
case "$ROW_GAP" in
  ""|0|1) ;;
  *) die "row-gap must be 0 or 1" ;;
esac
case "$QUOTA_PERCENT" in
  ""|remaining|used) ;;
  *) die "quota-percent must be remaining or used" ;;
esac
case "$BRAND_COLORS" in
  ""|on|off) ;;
  *) die "brand-colors must be on or off" ;;
esac
case "$BRAND_GLYPHS" in
  ""|icon|unicode|off) ;;
  *) die "brand-glyphs must be icon, unicode, or off" ;;
esac
case "$AGENT_ORDER" in
  ""|default|quota) ;;
  *) die "agent-order must be default or quota" ;;
esac
# `0` is accepted as a spelling of off, the same as configure reads it.
case "$LOW_QUOTA_ALERT" in
  ""|off) ;;
  *[!0-9]*) die "low-quota-alert must be off or a percentage from 0 to 100" ;;
  *) ((LOW_QUOTA_ALERT <= 100)) \
    || die "low-quota-alert must be off or a percentage from 0 to 100" ;;
esac
# The field list is validated by configure, which owns the field names.

printf '%s\n' '→ building QuotaDeck'
cargo build --release --locked --manifest-path "$ROOT/Cargo.toml"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    cp "$ROOT/target/release/herdr-agent-quota.exe" \
      "$ROOT/target/release/herdr-agent-quota"
    ;;
esac
HERDR_ACTION_JSON_PARSER="$ROOT/target/release/herdr-agent-quota"
export HERDR_ACTION_JSON_PARSER

printf '%s\n' '→ linking and enabling the Herdr plugin'
link_output="$(herdr plugin link "$ROOT" --enabled 2>&1)" || die "herdr plugin link failed: $link_output"

# Herdr runs a plugin action with a fixed command line, in the server's own
# environment: variables exported around `herdr plugin action invoke` never
# reach it. The plugin config directory is the only channel that does, so every
# option travels through a file there rather than through `env`.
write_plugin_pref() {
  local name="$1" value="$2"
  [[ -z "$value" ]] && return 0
  local directory
  directory="$(herdr plugin config-dir "$HERDR_ACTION_PLUGIN_ID")" \
    || die "cannot resolve plugin config directory"
  mkdir -p "$directory"
  printf '%s\n' "$value" > "$directory/$name"
  # Proxy URLs can include credentials; also tighten an existing file.
  if [[ "$name" == http-proxy ]]; then chmod 600 "$directory/$name"; fi
}

write_plugin_pref agents "$AGENTS"
write_plugin_pref watch-interval-seconds "$WATCH_INTERVAL_SECONDS"
write_plugin_pref sidebar-layout "$SIDEBAR_LAYOUT"
write_plugin_pref row-gap "$ROW_GAP"
write_plugin_pref quota-percent "$QUOTA_PERCENT"
write_plugin_pref fields "$FIELDS"
write_plugin_pref brand-colors "$BRAND_COLORS"
write_plugin_pref brand-glyphs "$BRAND_GLYPHS"
write_plugin_pref agent-order "$AGENT_ORDER"
write_plugin_pref low-quota-alert "$LOW_QUOTA_ALERT"
(umask 077; write_plugin_pref http-proxy "$QUOTA_HTTP_PROXY")

printf '%s\n' '→ installing reversible sidebar and provider collectors'
invoke_action_and_wait configure || die "configuration action failed"

# The action's own report, including which providers are already signed in.
# Best effort: a Herdr that keeps no stdout just skips it.
herdr_action_stdout "$HERDR_ACTION_LAST_LOG_ID" 2>/dev/null || true

printf '%s\n' 'Installed. Restart already-running agent sessions once so they load the refreshed hooks.'
printf '%s\n' 'Run `quotadeck setup` any time for a guided sign-in to the providers above.'
