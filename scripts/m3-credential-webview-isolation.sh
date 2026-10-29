#!/usr/bin/env bash
set -euo pipefail
umask 077

if [ "$#" -ne 7 ]; then
  printf '%s\n' 'Usage: m3-credential-webview-isolation.sh DAEMON DAEMON_LIB_DIR DRIVER DRIVER_LIB_DIR CLIENT SERVER SCENARIO' >&2
  exit 2
fi
for tool in bwrap timeout dbus-run-session dbus-daemon gdbus Xvfb python3 realpath; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf '%s\n' 'A required isolation or WebView tool is unavailable.' >&2
    exit 2
  fi
done
resolve_input() {
  realpath -e -- "$1" 2>/dev/null || { printf '%s\n' 'An isolation test input is unavailable.' >&2; return 1; }
}
test_daemon=$(resolve_input "$1")
test_libraries=$(resolve_input "$2")
test_driver=$(resolve_input "$3")
test_driver_libraries=$(resolve_input "$4")
test_client=$(resolve_input "$5")
test_server=$(resolve_input "$6")
test_scenario=$(resolve_input "$7")
test_helper=$(resolve_input "$(dirname -- "$test_scenario")/m2-workspace-webdriver.py")
test_bwrap=$(command -v bwrap)
if [[ "$test_bwrap" != /* ]] || [ ! -x "$test_daemon" ] || [ ! -d "$test_libraries" ] || [ ! -x "$test_driver" ] || [ ! -d "$test_driver_libraries" ] ||
   [ ! -x "$test_client" ] || [ ! -x "$test_server" ] || [ ! -r "$test_scenario" ] || [ ! -r "$test_helper" ]; then
  printf '%s\n' 'An isolation test input is unavailable.' >&2
  exit 2
fi
case "$test_daemon" in /tmp/*|/dev/shm/*) ;; *) printf '%s\n' 'The daemon must be a temporary extracted test copy.' >&2; exit 2 ;; esac
case "$test_libraries" in /tmp/*|/dev/shm/*) ;; *) printf '%s\n' 'The library directory must be a temporary extracted test copy.' >&2; exit 2 ;; esac
case "$test_driver" in /tmp/*|/dev/shm/*) ;; *) printf '%s\n' 'The driver must be a temporary extracted test copy.' >&2; exit 2 ;; esac
case "$test_driver_libraries" in /tmp/*|/dev/shm/*) ;; *) printf '%s\n' 'The driver libraries must be a temporary extracted test copy.' >&2; exit 2 ;; esac
if [ ! -r /etc/ssl/certs/ca-certificates.crt ]; then
  printf '%s\n' 'The system CA bundle is unavailable.' >&2
  exit 2
fi
test_user_dir=${HOME-}
if [[ "$test_user_dir" != /home/* || ! -d "$test_user_dir" ]]; then
  printf '%s\n' 'The test requires a user directory below /home.' >&2
  exit 2
fi
test_user_resolved=$(resolve_input "$test_user_dir")
if [[ "$test_user_resolved" != /home/* ]]; then
  printf '%s\n' 'The user directory must resolve below /home.' >&2
  exit 2
fi

test_dir=$(mktemp -d /tmp/notryx-credential-webview.XXXXXXXX)
cleanup() {
  if [[ "$test_dir" == /tmp/notryx-credential-webview.* && -d "$test_dir" && ! -L "$test_dir" ]]; then
    rm -rf -- "$test_dir" || return 1
  fi
  [ ! -e "$test_dir" ] && [ ! -L "$test_dir" ]
}
trap 'test_status=$?; trap - EXIT; if ! cleanup; then printf "%s\n" "Temporary test cleanup failed." >&2; exit 1; fi; exit "$test_status"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -m 700 -- "$test_dir/run" "$test_dir/tmp" "$test_dir/xdg-data" "$test_dir/xdg-config" "$test_dir/xdg-cache" "$test_dir/control"
printf '%s\n' '0123456789abcdef0123456789abcdef' > "$test_dir/machine-id"
cp -- /etc/ssl/certs/ca-certificates.crt "$test_dir/ca-bundle"
chmod 400 "$test_dir/ca-bundle"
cat > "$test_dir/bus.conf" <<'BUS'
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=/work/run/bus.sock</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
BUS
cat > "$test_dir/inner.sh" <<'INNER'
#!/usr/bin/env bash
set -euo pipefail
umask 077
case "${DBUS_SESSION_BUS_ADDRESS-}" in
  'unix:path=/work/run/bus.sock'|'unix:path=/work/run/bus.sock,'*) ;;
  *) exit 1 ;;
esac
daemon_pid=
xvfb_pid=
cleanup_children() {
  if [ -n "$xvfb_pid" ]; then kill "$xvfb_pid" 2>/dev/null || true; wait "$xvfb_pid" 2>/dev/null || true; fi
  if [ -n "$daemon_pid" ]; then kill "$daemon_pid" 2>/dev/null || true; wait "$daemon_pid" 2>/dev/null || true; fi
}
trap cleanup_children EXIT
printf '%s\n' daemon > /work/stage
printf '%s\n' 'notryx-synthetic-keyring-password' |
  env LD_LIBRARY_PATH=/work/keyring-libs /work/keyring-daemon --foreground --unlock --components=secrets --control-directory=/work/control > /work/daemon.log 2>&1 &
daemon_pid=$!
ready=0
for ((attempt = 0; attempt < 100; attempt++)); do
  if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner org.freedesktop.secrets > /work/readiness.log 2>&1 && grep -Fxq '(true,)' /work/readiness.log; then ready=1; break; fi
  kill -0 "$daemon_pid" 2>/dev/null || break
  sleep 0.1
done
[ "$ready" = 1 ]
printf '%s\n' display > /work/stage
Xvfb :99 -screen 0 1280x800x24 -nolisten tcp -ac > /work/xvfb.log 2>&1 &
xvfb_pid=$!
export DISPLAY=:99
ready=0
for ((attempt = 0; attempt < 100; attempt++)); do
  if [ -S /tmp/.X11-unix/X99 ]; then ready=1; break; fi
  kill -0 "$xvfb_pid" 2>/dev/null || break
  sleep 0.1
done
[ "$ready" = 1 ]
printf '%s\n' scenario > /work/stage
python3 /work/scripts/m3-credential-restart-webdriver.py --binary /work/notryx-client --server /work/notryx-server --driver /work/webdriver > /work/scenario.log 2>&1
printf '%s\n' complete > /work/stage
INNER
chmod 700 "$test_dir/inner.sh"
cat > "$test_dir/webdriver" <<'DRIVER'
#!/usr/bin/env bash
exec env LD_LIBRARY_PATH=/work/driver-libs /work/driver-bin "$@"
DRIVER
chmod 700 "$test_dir/webdriver"

test_result=0
env -i PATH=/usr/bin:/bin timeout --signal=TERM --kill-after=5s 300s python3 -I -S -c '
import errno, os, sys
for name in os.listdir("/proc/self/fd"):
    if name.isdecimal() and int(name) > 2:
        try:
            os.close(int(name))
        except OSError as error:
            if error.errno != errno.EBADF:
                raise
os.execv(sys.argv[1], sys.argv[1:])
' "$test_bwrap" \
  --die-with-parent --unshare-user --unshare-pid --unshare-ipc --unshare-net --new-session --uid 0 --gid 0 --cap-add CAP_NET_ADMIN \
  --ro-bind /usr /usr --ro-bind-try /bin /bin --ro-bind-try /lib /lib --ro-bind-try /lib64 /lib64 \
  --dir /etc --ro-bind /etc/passwd /etc/passwd --ro-bind /etc/group /etc/group \
  --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
  --ro-bind "$test_dir/machine-id" /etc/machine-id --bind "$test_dir" /work \
  --ro-bind "$test_daemon" /work/keyring-daemon --ro-bind "$test_libraries" /work/keyring-libs \
  --ro-bind "$test_driver" /work/driver-bin --ro-bind "$test_driver_libraries" /work/driver-libs \
  --ro-bind "$test_client" /work/notryx-client --ro-bind "$test_server" /work/notryx-server \
  --dir /work/scripts --ro-bind "$test_scenario" /work/scripts/m3-credential-restart-webdriver.py --ro-bind "$test_helper" /work/scripts/m2-workspace-webdriver.py \
  --proc /proc --dev /dev --tmpfs /tmp --tmpfs /run --tmpfs /home --dir /root --dir "$test_user_dir" --dir "$test_user_resolved" \
  --chdir /work --clearenv --setenv PATH /usr/bin:/bin --setenv TMPDIR /work/tmp \
  --setenv XDG_DATA_HOME /work/xdg-data --setenv XDG_CONFIG_HOME /work/xdg-config --setenv XDG_CACHE_HOME /work/xdg-cache --setenv XDG_RUNTIME_DIR /work/run \
  --setenv SSL_CERT_FILE /work/ca-bundle --setenv NOTRYX_TEST_SANDBOX credential-integration \
  -- python3 -I -S -c '
import fcntl, socket, struct, subprocess
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
request = struct.pack("16sH14x", b"lo", 0)
flags = struct.unpack("16sH14x", fcntl.ioctl(sock, 0x8913, request))[1]
fcntl.ioctl(sock, 0x8914, struct.pack("16sH14x", b"lo", flags | 1))
sock.close()
subprocess.run(["dbus-run-session", "--config-file=/work/bus.conf", "--", "/work/inner.sh"], check=True)
' </dev/null > "$test_dir/session.log" 2>&1 || test_result=$?

if [ "$test_result" -ne 0 ]; then
  test_stage=setup
  if [ -f "$test_dir/stage" ]; then read -r test_stage < "$test_dir/stage" || test_stage=setup; fi
  case "$test_stage" in setup|daemon|display|scenario) ;; *) test_stage=setup ;; esac
  printf 'Isolated credential WebView test failed at %s. Captured output is withheld.\n' "$test_stage" >&2
  if [ "$test_stage" = scenario ] && [ -f "$test_dir/scenario-stage" ]; then
    test_scenario_stage=unknown
    read -r test_scenario_stage < "$test_dir/scenario-stage" || test_scenario_stage=unknown
    case "$test_scenario_stage" in startup|pair|checkpoint|app_restart|server_restart|delivery|ack|complete) ;; *) test_scenario_stage=unknown ;; esac
    printf 'Scenario stage: %s\n' "$test_scenario_stage" >&2
  fi
  case "$test_result" in
    124) printf '%s\n' 'Failure category: timeout' >&2 ;;
    137) printf '%s\n' 'Failure category: terminated' >&2 ;;
    *) printf '%s\n' 'Failure category: execution' >&2 ;;
  esac
  exit 1
fi
if [ "$(cat "$test_dir/stage")" != complete ]; then
  printf '%s\n' 'Isolated credential WebView test did not complete.' >&2
  exit 1
fi
printf '%s\n' 'Isolated credential WebView scenario completed.'
