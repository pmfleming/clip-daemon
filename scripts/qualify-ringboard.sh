echo "Daemon capture / Ringboard storage qualification (read-only; no service changes)"
echo "WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-unset}"
echo "XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-unset}"
printf 'ringboard-server: '; command -v ringboard-server || true
printf 'clip-daemon: '; command -v clip-daemon || true
echo 'Expected services: clip-daemon + ringboard-server; the legacy watcher must be disabled/masked.'

protocols=$(wayland-info 2>/dev/null || true)
check_protocol() {
  protocol=$1
  if printf '%s' "$protocols" | grep -q "$protocol"; then
    printf 'PASS protocol %s\n' "$protocol"
  else
    printf 'FAIL protocol %s\n' "$protocol"
  fi
}
if printf '%s' "$protocols" | grep -q ext_data_control_manager_v1; then
  echo 'PASS capture protocol ext_data_control_manager_v1 (preferred)'
elif printf '%s' "$protocols" | grep -q zwlr_data_control_manager_v1; then
  echo 'CHECK capture protocol wlr-data-control: v2 required; real-compositor fallback qualification pending'
else
  echo 'FAIL no supported capture protocol'
fi
check_protocol zwp_virtual_keyboard_manager_v1
if printf '%s' "$protocols" | grep -Eq 'ext_foreign_toplevel_list_v1|zwlr_foreign_toplevel_manager_v1'; then
  echo 'PASS protocol foreign-toplevel'
else
  echo 'FAIL protocol foreign-toplevel'
fi

runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
data=${XDG_DATA_HOME:-$HOME/.local/share}/clipboard-history
printf 'runtime directory: %s\n' "$runtime"
printf 'database directory: %s (%s)\n' "$data" "$([ -r "$data" ] && echo readable || echo absent)"
echo 'Remaining MIME, sensitive-data, focus, size-limit, and source-exit checks require the documented hardware run.'
