#!/usr/bin/env bash
# install-memory-keeper.sh — manage memory-keeper instances across projects.
#
# One memory-keeper per project: the canonical prompt lives in THIS repo
# (docs/agents/memory-keeper.md). This script distributes it, writes each
# project's opencode agent config with its bus partition baked in, and
# registers a launchd schedule per project.
#
# Usage:
#   install-memory-keeper.sh install <repo_root> <partition> [--hours 8,17]
#   install-memory-keeper.sh update                    # re-copy prompt to all installed
#   install-memory-keeper.sh list                     # show instances
#   install-memory-keeper.sh uninstall <partition>    # remove instance + schedule
#
# launchd label: com.<partition>.memorykeeper
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CANONICAL_PROMPT="$HERE/memory-keeper.md"
LAUNCH_AGENTS_DIR="$HOME/Library/LaunchAgents"
LOGS_DIR="$HOME/Library/Logs"
OPENCODE_BIN="$(command -v opencode || echo "$HOME/.bun/bin/opencode")"

fail() { echo "error: $*" >&2; exit 1; }

canonical_partition() {
  # The partition must match the project's agent-bus partition. Derive a
  # default from the repo folder name (lowercased, non-alnum -> _) but the
  # caller should pass it explicitly when the project uses a different name.
  basename "$1" | tr '[:upper:]' '[:lower:]' | tr -c '[:alnum:]' '_'
}

write_agent_config() {
  local repo="$1" partition="$2"
  mkdir -p "$repo/.opencode/agents" "$repo/docs/agents"
  sed "s/<project>/$partition/g" "$HERE/agent-config.template.md" > "$repo/.opencode/agents/memory-keeper.md"
  if [[ "$repo/docs/agents/memory-keeper.md" != "$CANONICAL_PROMPT" ]]; then
    cp "$CANONICAL_PROMPT" "$repo/docs/agents/memory-keeper.md"
  fi
}

write_plist() {
  local repo="$1" partition="$2" hours="$3"
  local plist="$LAUNCH_AGENTS_DIR/com.$partition.memorykeeper.plist"
  mkdir -p "$LAUNCH_AGENTS_DIR" "$LOGS_DIR"
  local first="${hours%%,*}" second="${hours#*,}"
  cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.$partition.memorykeeper</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/zsh</string>
		<string>-lc</string>
		<string>$OPENCODE_BIN run --agent memory-keeper --dir $repo "daily docs sweep"</string>
	</array>
	<key>StartCalendarInterval</key>
	<array>
		<dict>
			<key>Hour</key>
			<integer>$first</integer>
			<key>Minute</key>
			<integer>0</integer>
		</dict>
		<dict>
			<key>Hour</key>
			<integer>$second</integer>
			<key>Minute</key>
			<integer>0</integer>
		</dict>
	</array>
	<key>StandardOutPath</key>
	<string>$LOGS_DIR/$partition-memorykeeper.log</string>
	<key>StandardErrorPath</key>
	<string>$LOGS_DIR/$partition-memorykeeper.err.log</string>
	<key>RunAtLoad</key>
	<false/>
	<key>MemoryKeeperRepo</key>
	<string>$repo</string>
</dict>
</plist>
EOF
  plutil -lint "$plist" >/dev/null
}

load_or_reload() {
  local partition="$1"
  local plist="$LAUNCH_AGENTS_DIR/com.$partition.memorykeeper.plist"
  launchctl bootout "gui/$(id -u)/com.$partition.memorykeeper" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$plist"
}

installed_partitions() {
  ls "$LAUNCH_AGENTS_DIR"/com.*.memorykeeper.plist 2>/dev/null | sed -E 's|.*/com\.(.*)\.memorykeeper\.plist|\1|' || true
}

cmd_install() {
  [[ $# -ge 2 ]] || fail "install <repo_root> <partition> [--hours 8,17]"
  local repo="$(cd "$1" && pwd)"
  local partition="$2"
  local hours="8,17"
  shift 2
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --hours) hours="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  [[ -d "$repo/docs" ]] || fail "$repo has no docs/ — not a doc-bearing repo?"
  [[ -f "$repo/docs/agents/memory-keeper.md" ]] || mkdir -p "$repo/docs/agents"
  write_agent_config "$repo" "$partition"
  write_plist "$repo" "$partition" "$hours"
  load_or_reload "$partition"
  echo "installed memory-keeper for $partition -> $repo (daily $hours, label com.$partition.memorykeeper)"
}

cmd_update() {
  local n=0
  for partition in $(installed_partitions); do
    local repo
    repo="$(defaults read "$LAUNCH_AGENTS_DIR/com.$partition.memorykeeper.plist" MemoryKeeperRepo 2>/dev/null)" || true
    if [[ -n "$repo" && -d "$repo" ]]; then
      write_agent_config "$repo" "$partition"
      n=$((n + 1))
      echo "updated $partition -> $repo"
    else
      echo "skip $partition (repo not found at $repo)" >&2
    fi
  done
  echo "updated $n instance(s)"
}

cmd_list() {
  local found=0
  for partition in $(installed_partitions); do
    found=1
    local state
    state="$(launchctl list 2>/dev/null | awk -v p="com.$partition.memorykeeper" '$3==p {print "loaded"}')"
    echo "$partition: ${state:-not loaded}"
  done
  [[ $found -eq 1 ]] || echo "no memory-keeper instances installed"
}

cmd_uninstall() {
  [[ $# -ge 1 ]] || fail "uninstall <partition>"
  local partition="$1"
  launchctl bootout "gui/$(id -u)/com.$partition.memorykeeper" 2>/dev/null || true
  rm -f "$LAUNCH_AGENTS_DIR/com.$partition.memorykeeper.plist"
  echo "removed launchd schedule for $partition (agent files left in the repo)"
}

case "${1:-}" in
  install) shift; cmd_install "$@" ;;
  update) cmd_update ;;
  list) cmd_list ;;
  uninstall) shift; cmd_uninstall "$@" ;;
  *) sed -n '2,12p' "$0" ;;
esac
