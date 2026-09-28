# Invoke a Herdr plugin action and wait for it to finish.
#
# `herdr plugin action invoke` starts the action and returns its log id before
# the action completes. Both installer scripts need the matching log to report
# success before they continue: uninstall.sh unlinks the plugin next, and
# unlinking while the restore action is still running can leave a statusLine
# entry pointing at a plugin that is no longer there.
#
# Sourced by install.sh and uninstall.sh; not a standalone script.

HERDR_ACTION_PLUGIN_ID="herdr-agent-quota-win"
# Configuration writes touch a handful of small files. A minute is far beyond
# any legitimate run and still bounds a hung action.
HERDR_ACTION_TIMEOUT_SECONDS="${HERDR_ACTION_TIMEOUT_SECONDS:-60}"

# Status of one log entry, or empty while Herdr has not listed it yet.
herdr_action_status() {
  local output
  output="$(herdr plugin log list --plugin "$HERDR_ACTION_PLUGIN_ID" --limit 50 2>/dev/null)" \
    || return 1
  printf '%s' "$output" \
    | "$HERDR_ACTION_JSON_PARSER" parse-herdr-action-json --log-id "$1"
}

# Captured stdout of one finished log entry; empty if Herdr kept none.
herdr_action_stdout() {
  local output
  output="$(herdr plugin log list --plugin "$HERDR_ACTION_PLUGIN_ID" --limit 50 2>/dev/null)" \
    || return 1
  printf '%s' "$output" \
    | "$HERDR_ACTION_JSON_PARSER" parse-herdr-action-json --log-id "$1" --stdout
}

# invoke_action_and_wait <action-id>
#
# Returns non-zero unless the matching action log explicitly reports success.
# The log id is left in HERDR_ACTION_LAST_LOG_ID for herdr_action_stdout.
invoke_action_and_wait() {
  # `status` is a read-only special parameter in zsh, so this stays `state`
  # even though the scripts themselves run under bash.
  local action="$1" output log_id state waited=0

  output="$(herdr plugin action invoke "$HERDR_ACTION_PLUGIN_ID.$action")" || return 1
  [[ -x "$HERDR_ACTION_JSON_PARSER" ]] || {
    printf 'error: QuotaDeck action JSON parser is unavailable\n' >&2
    return 1
  }
  log_id="$(printf '%s' "$output" | "$HERDR_ACTION_JSON_PARSER" parse-herdr-action-json)" \
    || return 1
  [[ -n "$log_id" ]] || return 1
  HERDR_ACTION_LAST_LOG_ID="$log_id"

  while ((waited < HERDR_ACTION_TIMEOUT_SECONDS)); do
    state="$(herdr_action_status "$log_id")" || {
      printf 'error: cannot verify plugin action %s\n' "$action" >&2
      return 1
    }
    case "$state" in
      running|"") ;;
      succeeded) return 0 ;;
      *)
        printf 'error: plugin action %s %s\n' "$action" "$state" >&2
        printf 'inspect it with: herdr plugin log list --plugin %s\n' \
          "$HERDR_ACTION_PLUGIN_ID" >&2
        return 1
        ;;
    esac
    sleep 1
    waited=$((waited + 1))
  done

  printf 'error: plugin action %s did not finish within %ss\n' \
    "$action" "$HERDR_ACTION_TIMEOUT_SECONDS" >&2
  return 1
}
