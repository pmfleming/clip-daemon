#!/usr/bin/env bash
set -euo pipefail

mode=${1:-check}
state_dir=${XDG_STATE_HOME:-$HOME/.local/state}/clip-daemon
report=${CLIP_DAEMON_ACCEPTANCE_REPORT:-$state_dir/hardware-acceptance.tsv}
mkdir -p "$(dirname "$report")"
touch "$report"
chmod 600 "$report"

checks=(
  image-round-trip
  file-round-trip
  file-mime-priority
  paste-target
  sensitive-selection
  oversized-source
  source-exit-persistence
)

has_check() {
  local requested=$1 candidate
  for candidate in "${checks[@]}"; do [[ $candidate == "$requested" ]] && return 0; done
  return 1
}

record() {
  local check=$1 result=$2 notes=${3:-}
  has_check "$check" || { echo "Unknown check: $check" >&2; exit 2; }
  [[ $result == pass || $result == fail || $result == blocked ]] || {
    echo "Result must be pass, fail, or blocked" >&2; exit 2;
  }
  notes=${notes//$'\t'/ }
  notes=${notes//$'\n'/ }
  local temp
  temp=$(mktemp "${report}.XXXXXX")
  awk -F '\t' -v check="$check" '$1 != check' "$report" > "$temp"
  printf '%s\t%s\t%s\t%s\n' "$check" "$result" "$(date --iso-8601=seconds)" "$notes" >> "$temp"
  chmod 600 "$temp"
  mv "$temp" "$report"
}

show() {
  local check line
  printf '%-25s %-8s %s\n' CHECK RESULT NOTES
  for check in "${checks[@]}"; do
    line=$(awk -F '\t' -v check="$check" '$1 == check { line=$0 } END { print line }' "$report")
    if [[ -n $line ]]; then
      IFS=$'\t' read -r _ result timestamp notes <<< "$line"
      printf '%-25s %-8s %s (%s)\n' "$check" "$result" "$notes" "$timestamp"
    else
      printf '%-25s %-8s %s\n' "$check" pending "Run: $0 record $check pass|fail|blocked [notes]"
    fi
  done
}

case "$mode" in
  check)
    bash "$(dirname "$0")/qualify-ringboard.sh"
    echo
    show
    ;;
  show) show ;;
  record)
    [[ $# -ge 3 ]] || { echo "Usage: $0 record CHECK RESULT [NOTES]" >&2; exit 2; }
    record "$2" "$3" "${4:-}"
    show
    ;;
  *) echo "Usage: $0 [check|show|record CHECK RESULT [NOTES]]" >&2; exit 2 ;;
esac
