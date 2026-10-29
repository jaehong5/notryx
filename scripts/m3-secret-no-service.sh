#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -eq 0 ]; then
  printf '%s\n' 'Usage: scripts/m3-secret-no-service.sh COMMAND [ARGUMENTS...]' >&2
  exit 2
fi

test_bus_dir=$(mktemp -d /tmp/notryx-secret-test.XXXXXXXX)
cleanup() {
  if [[ "$test_bus_dir" == /tmp/notryx-secret-test.* && -d "$test_bus_dir" && ! -L "$test_bus_dir" ]]; then
    rm -rf -- "$test_bus_dir"
  fi
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

cat >"$test_bus_dir/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$test_bus_dir/bus.sock</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF

unset DBUS_SESSION_BUS_ADDRESS DBUS_SESSION_BUS_PID DBUS_STARTER_ADDRESS DBUS_STARTER_BUS_TYPE
export NOTRYX_TEST_SECRET_BUS_PREFIX="$test_bus_dir"
dbus-run-session --config-file="$test_bus_dir/bus.conf" -- bash -c '
  set -euo pipefail
  case "${DBUS_SESSION_BUS_ADDRESS-}" in
    "unix:path=$NOTRYX_TEST_SECRET_BUS_PREFIX/bus.sock"|"unix:path=$NOTRYX_TEST_SECRET_BUS_PREFIX/bus.sock,"*) ;;
    *) printf "%s\n" "The test session bus is not isolated." >&2; exit 1 ;;
  esac
  exec "$@"
' -- "$@"
