#!/usr/bin/env bash
set -euo pipefail
umask 077

if [ "$#" -ne 1 ]; then
  printf '%s\n' 'Usage: scripts/m3-secret-daemon-test.sh SYNTHETIC_TEST_BINARY' >&2
  exit 2
fi
if [ ! -f "$1" ] || [ ! -r "$1" ]; then
  printf '%s\n' 'The synthetic test binary is not readable.' >&2
  exit 2
fi
for required_tool in bwrap timeout dbus-run-session dbus-daemon gnome-keyring-daemon gdbus grep awk base64 od tr realpath python3; do
  if ! command -v "$required_tool" >/dev/null 2>&1; then
    printf 'Required test tool is unavailable: %s\n' "$required_tool" >&2
    exit 2
  fi
done
test_bwrap_path=$(command -v bwrap)
if [[ "$test_bwrap_path" != /* ]]; then
  printf '%s\n' 'The sandbox executable could not be resolved safely.' >&2
  exit 2
fi
if [ ! -f /etc/ssl/certs/ca-certificates.crt ] || [ ! -r /etc/ssl/certs/ca-certificates.crt ]; then
  printf '%s\n' 'The system CA bundle is unavailable or unreadable.' >&2
  exit 2
fi

test_home_dir=${HOME-}
if [[ "$test_home_dir" != /home/* || ! -d "$test_home_dir" ]]; then
  printf '%s\n' 'This isolated test requires the current user directory under /home.' >&2
  exit 2
fi
test_home_resolved=$(realpath -e -- "$test_home_dir")
if [[ "$test_home_resolved" != /home/* || "$test_home_resolved" == /home ]]; then
  printf '%s\n' 'The current user directory must resolve below /home.' >&2
  exit 2
fi

test_dir=$(mktemp -d /tmp/notryx-secret-daemon.XXXXXXXX)
cleanup() {
  if [[ "$test_dir" == /tmp/notryx-secret-daemon.* && -d "$test_dir" && ! -L "$test_dir" ]]; then
    rm -rf -- "$test_dir" || return 1
  fi
  [ ! -e "$test_dir" ] && [ ! -L "$test_dir" ]
}
trap 'test_exit_status=$?; trap - EXIT; if ! cleanup; then printf "%s\n" "Temporary test directory cleanup failed." >&2; exit 1; fi; exit "$test_exit_status"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -m 700 -- "$test_dir/data" "$test_dir/xdg-data" "$test_dir/xdg-config" "$test_dir/xdg-cache" "$test_dir/run" "$test_dir/control-before" "$test_dir/control-after"
printf '%s\n' '0123456789abcdef0123456789abcdef' >"$test_dir/machine-id"
cat >"$test_dir/bus.conf" <<'BUS'
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
cat >"$test_dir/inner.sh" <<'INNER'
#!/usr/bin/env bash
set -euo pipefail
umask 077

case "${DBUS_SESSION_BUS_ADDRESS-}" in
  'unix:path=/work/run/bus.sock'|'unix:path=/work/run/bus.sock,'*) ;;
  *) exit 1 ;;
esac

daemon_pid=
start_daemon() {
  printf '%s\n' 'notryx-synthetic-keyring-password' |
    gnome-keyring-daemon --foreground --unlock --components=secrets --control-directory="$1" >"$2" 2>&1 &
  daemon_pid=$!
}
stop_daemon() {
  if [ -n "$daemon_pid" ]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=
  fi
}
trap stop_daemon EXIT

wait_for_owner() {
  local expected=$1
  for ((attempt = 0; attempt < 100; attempt++)); do
    if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner org.freedesktop.secrets > /work/readiness.log 2>&1 && grep -Fxq "($expected,)" /work/readiness.log; then
      return 0
    fi
    if [ "$expected" = true ]; then
      kill -0 "$daemon_pid" 2>/dev/null || return 1
    fi
    sleep 0.1
  done
  return 1
}

printf '%s\n' daemon > /work/stage
start_daemon /work/control-before /work/daemon-before.log
printf '%s\n' readiness > /work/stage
wait_for_owner true

printf '%s\n' put > /work/stage
/work/test-binary credential_platform_daemon_put --ignored --test-threads=1 --nocapture > /work/put.log 2>&1
grep -Eq '^test result: ok\. 1 passed; 0 failed;' /work/put.log

printf '%s\n' restart > /work/stage
stop_daemon
wait_for_owner false
start_daemon /work/control-after /work/daemon-after.log
wait_for_owner true

printf '%s\n' get_delete > /work/stage
/work/test-binary credential_platform_daemon_get_delete --ignored --test-threads=1 > /work/get-delete.log 2>&1
grep -Eq '^test result: ok\. 1 passed; 0 failed;' /work/get-delete.log
printf '%s\n' stop > /work/stage
stop_daemon
wait_for_owner false
printf '%s\n' complete > /work/stage
INNER
chmod 700 "$test_dir/inner.sh"

test_result=0
env -i PATH=/usr/bin:/bin timeout --signal=TERM --kill-after=5s 120s bash -c '
  set -euo pipefail
  cp -- "$1" "$2/test-binary"
  chmod 700 "$2/test-binary"
  cp -- /etc/ssl/certs/ca-certificates.crt "$2/ca-bundle"
  chmod 400 "$2/ca-bundle"
  exec env -i PATH=/usr/bin:/bin python3 -I -S -c "
import errno
import os
import sys
for fd_name in os.listdir(\"/proc/self/fd\"):
    if fd_name.isdecimal() and int(fd_name) > 2:
        try:
            os.close(int(fd_name))
        except OSError as failure:
            if failure.errno != errno.EBADF:
                raise
os.execvp(sys.argv[1], sys.argv[1:])
" "$5" \
    --die-with-parent --unshare-user --unshare-pid --unshare-ipc --unshare-net --new-session \
    --ro-bind /usr /usr \
    --ro-bind-try /bin /bin --ro-bind-try /lib /lib --ro-bind-try /lib64 /lib64 \
    --dir /etc --ro-bind /etc/passwd /etc/passwd --ro-bind /etc/group /etc/group \
    --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf \
    --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
    --ro-bind "$2/machine-id" /etc/machine-id --bind "$2" /work \
    --proc /proc --dev /dev --tmpfs /tmp --tmpfs /run --tmpfs /home \
    --dir "$3" --dir "$4" \
    --chdir /work --clearenv \
    --setenv PATH /usr/bin:/bin \
    --setenv XDG_DATA_HOME /work/xdg-data --setenv XDG_CONFIG_HOME /work/xdg-config \
    --setenv XDG_CACHE_HOME /work/xdg-cache --setenv XDG_RUNTIME_DIR /work/run \
    --setenv SSL_CERT_FILE /work/ca-bundle \
    --setenv NOTRYX_TEST_SECRET_BUS_PREFIX /work --setenv NOTRYX_TEST_SECRET_DIR /work/data \
    -- dbus-run-session --config-file=/work/bus.conf -- /work/inner.sh
' -- "$1" "$test_dir" "$test_home_dir" "$test_home_resolved" "$test_bwrap_path" </dev/null >"$test_dir/session.log" 2>&1 || test_result=$?

test_marker='synthetic-secret-service-token-4bcaebd2'
test_marker_base64=$(printf '%s' "$test_marker" | base64 | tr -d '\n')
test_marker_hex=$(printf '%s' "$test_marker" | od -An -v -tx1 | tr -d ' \n')
test_decimal_bytes=$(printf '%s' "$test_marker" | od -An -v -tu1)
test_marker_decimal='['
test_decimal_separator=
while read -r -a test_byte_values; do
  for test_byte in "${test_byte_values[@]}"; do
    test_marker_decimal+="$test_decimal_separator$test_byte"
    test_decimal_separator=', '
  done
done <<<"$test_decimal_bytes"
test_marker_decimal+=']'
for test_log in "$test_dir"/*.log; do
  if [ ! -f "$test_log" ] || [ -L "$test_log" ]; then
    printf '%s\n' 'Captured test output is unavailable or unsafe; output was withheld.' >&2
    exit 1
  fi
  for marker_value in "$test_marker" "$test_marker_base64" "$test_marker_hex" "$test_marker_decimal"; do
    if grep -Fq -- "$marker_value" "$test_log"; then
      printf '%s\n' 'Synthetic credential appeared in captured test output; output was withheld.' >&2
      exit 1
    else
      grep_result=$?
      if [ "$grep_result" -ne 1 ]; then
        printf '%s\n' 'Captured test output could not be checked; output was withheld.' >&2
        exit 1
      fi
    fi
  done
done

read_fixed_marker() {
  local marker_prefix=$1
  local marker_log=$2
  local marker_category
  local selected_marker=unknown
  local marker_count=0
  local marker_grep_result
  shift 2
  for marker_category in "$@"; do
    if grep -Fxq -- "$marker_prefix=$marker_category" "$marker_log" 2>/dev/null; then
      selected_marker=$marker_category
      marker_count=$((marker_count + 1))
    else
      marker_grep_result=$?
      if [ "$marker_grep_result" -ne 1 ]; then
        return 1
      fi
    fi
  done
  if [ "$marker_count" -eq 1 ]; then
    printf '%s' "$selected_marker"
  else
    printf '%s' unknown
  fi
}

if [ "$test_result" -ne 0 ]; then
  test_stage=setup
  if [ -f "$test_dir/stage" ] && [ ! -L "$test_dir/stage" ]; then
    test_stage=$(head -c 32 "$test_dir/stage")
  fi
  case "$test_stage" in daemon|readiness|put|restart|get_delete|stop|complete|setup) ;; *) test_stage=unknown ;; esac
  printf 'Isolated Secret Service test failed at %s; captured output was withheld.\n' "$test_stage" >&2
  if [ "$test_stage" = put ] && [ -f "$test_dir/put.log" ] && [ ! -L "$test_dir/put.log" ]; then
    if ! test_progress=$(awk '
      /NOTRYX_TEST_PROGRESS=(entered|isolation_ok|sqlite_open|draft_saved|payload_decoded|session_built|credential_ready|probe_ok|stored|sqlite_checked)\r?$/ {
        progress = $0
        sub(/^.*NOTRYX_TEST_PROGRESS=/, "", progress)
        sub(/\r$/, "", progress)
      }
      END { if (progress == "") print "none"; else print progress }
    ' "$test_dir/put.log" 2>/dev/null); then
      printf '%s\n' 'Captured test progress could not be classified.' >&2
      exit 1
    fi
    case "$test_progress" in
      entered|isolation_ok|sqlite_open|draft_saved|payload_decoded|session_built|credential_ready|probe_ok|stored|sqlite_checked)
        printf 'Test progress: %s\n' "$test_progress" >&2 ;;
      none) ;;
      *) printf '%s\n' 'Captured test progress could not be classified.' >&2; exit 1 ;;
    esac
    if ! probe_result=$(read_fixed_marker NOTRYX_TEST_PROBE_ERROR "$test_dir/put.log" connect no_default locked state); then
      printf '%s\n' 'Captured test output could not be classified.' >&2
      exit 1
    fi
    if [ "$probe_result" != unknown ]; then
      printf 'Secret Service probe: %s\n' "$probe_result" >&2
    fi
    if ! credential_result=$(read_fixed_marker NOTRYX_TEST_CREDENTIAL_ERROR "$test_dir/put.log" decode invalid_origin binding contract network encode other); then
      printf '%s\n' 'Captured test output could not be classified.' >&2
      exit 1
    fi
    if [ "$credential_result" != unknown ]; then
      printf 'Credential construction: %s\n' "$credential_result" >&2
    fi
    if ! stage_result=$(read_fixed_marker NOTRYX_TEST_STAGE_ERROR "$test_dir/put.log" storage unavailable binding_mismatch ambiguous missing write_unknown session_only); then
      printf '%s\n' 'Captured test output could not be classified.' >&2
      exit 1
    fi
    if [ "$stage_result" != unknown ]; then
      printf 'Credential stage: %s\n' "$stage_result" >&2
    else
      loader_result=1
      if grep -Fq -e 'error while loading shared libraries:' -e 'cannot execute: required file not found' -- "$test_dir/put.log" 2>/dev/null; then
        loader_result=0
      else
        loader_result=$?
      fi
      if [ "$loader_result" -eq 0 ]; then
        printf '%s\n' 'Test failure category: dynamic_loader' >&2
      elif [ "$loader_result" -eq 1 ]; then
        if [ "$test_progress" = none ]; then
          if grep -Fxq -- 'running 0 tests' "$test_dir/put.log" 2>/dev/null; then
            printf '%s\n' 'Test failure category: running_0_tests' >&2
          else
            test_count_result=$?
            if [ "$test_count_result" -ne 1 ]; then
              printf '%s\n' 'Captured test output could not be classified.' >&2
            elif grep -Fxq -- 'running 1 test' "$test_dir/put.log" 2>/dev/null; then
              printf '%s\n' 'Test failure category: running_1_test' >&2
            else
              test_count_result=$?
              if [ "$test_count_result" -eq 1 ]; then
                printf '%s\n' 'Test failure category: test_binary_failure' >&2
              else
                printf '%s\n' 'Captured test output could not be classified.' >&2
              fi
            fi
          fi
        else
          printf '%s\n' 'Test failure category: test_binary_failure' >&2
        fi
      else
        printf '%s\n' 'Captured test output could not be classified.' >&2
      fi
    fi
  fi
  exit 1
fi
if [ ! -f "$test_dir/stage" ] || [ -L "$test_dir/stage" ] || [ "$(cat "$test_dir/stage")" != complete ]; then
  printf '%s\n' 'The isolated Secret Service test did not complete; captured output was withheld.' >&2
  exit 1
fi
if ! cleanup; then
  trap - EXIT
  printf '%s\n' 'Temporary test directory cleanup failed.' >&2
  exit 1
fi
trap - EXIT
printf '%s\n' '격리 Secret Service put과 get-delete 시험이 통과했습니다. 확인한 원문, 표준 base64, 소문자 hex, 십진수 배열 표현이 캡처 로그에 없습니다.'
