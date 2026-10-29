#!/usr/bin/env bash
set -euo pipefail
umask 077

if [ "$#" -ne 6 ]; then
  printf '%s\n' '사용법: m3-credential-manual-isolation.sh WAYLAND_SOCKET DAEMON DAEMON_LIB_DIR CLIENT SERVER FLOW' >&2
  exit 2
fi
if [ ! -t 0 ] || [ ! -t 1 ] || [ ! -t 2 ]; then
  printf '%s\n' '이 시험은 사용자가 직접 사용하는 터미널에서 실행해야 합니다.' >&2
  exit 2
fi
for tool in bwrap timeout dbus-run-session dbus-daemon gdbus python3 realpath stat id; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf '%s\n' '필요한 격리 시험 도구를 찾지 못했습니다.' >&2
    exit 2
  fi
done
if ! env -i PATH=/usr/bin:/bin python3 -I -S -c '
import os, stat
try:
    items = [os.fstat(fd) for fd in (0, 1, 2)]
except OSError:
    raise SystemExit(1)
if not all(stat.S_ISCHR(item.st_mode) for item in items):
    raise SystemExit(1)
identity = lambda item: (item.st_dev, item.st_ino, item.st_rdev)
if len({identity(item) for item in items}) != 1:
    raise SystemExit(1)
'; then
  printf '%s\n' '표준 입출력이 같은 터미널에 연결돼 있어야 합니다.' >&2
  exit 2
fi
resolve_input() {
  realpath -e -- "$1" 2>/dev/null || { printf '%s\n' '시험 입력을 확인하지 못했습니다.' >&2; return 1; }
}
test_socket=$(resolve_input "$1")
test_daemon=$(resolve_input "$2")
test_libraries=$(resolve_input "$3")
test_client=$(resolve_input "$4")
test_server=$(resolve_input "$5")
test_flow=$(resolve_input "$6")
test_bwrap=$(command -v bwrap)
test_uid=$(id -u)
if [[ "$test_bwrap" != /* ]] || [ ! -S "$test_socket" ] || [ "$(stat -c %u -- "$test_socket" 2>/dev/null)" != "$test_uid" ] ||
   [ ! -x "$test_daemon" ] || [ ! -d "$test_libraries" ] || [ ! -x "$test_client" ] ||
   [ ! -x "$test_server" ] || [ ! -r "$test_flow" ]; then
  printf '%s\n' '시험 입력 또는 Wayland 소켓을 확인하지 못했습니다.' >&2
  exit 2
fi
case "$test_socket" in
  /run/user/"$test_uid"/wayland-*) ;;
  *) printf '%s\n' '선택한 사용자 Wayland 소켓만 지원합니다. X11은 지원하지 않습니다.' >&2; exit 2 ;;
esac
case "$test_daemon" in /tmp/*|/dev/shm/*) ;; *) printf '%s\n' '임시 추출 daemon이 필요합니다.' >&2; exit 2 ;; esac
case "$test_libraries" in /tmp/*|/dev/shm/*) ;; *) printf '%s\n' '임시 추출 라이브러리가 필요합니다.' >&2; exit 2 ;; esac
if [ ! -r /etc/ssl/certs/ca-certificates.crt ]; then
  printf '%s\n' '시스템 CA 묶음을 읽을 수 없습니다.' >&2
  exit 2
fi

printf '%s\n' 'Wayland compositor 소켓을 시험 앱에 공유합니다. compositor가 허용한 화면 및 클립보드 접근은 격리되지 않습니다.' >&2
printf '%s\n' '시험은 합성 서버와 임시 자료를 사용하며 최대 30분 후 종료합니다.' >&2
printf '%s\n' '합성 등록 코드가 표시되므로 기록하지 않는 직접 터미널에서 실행하세요. 터미널 기록기와 스크롤 기록은 차단하지 않습니다.' >&2
test_dir=$(mktemp -d /tmp/notryx-credential-manual.XXXXXXXX)
cleanup() {
  if [[ "$test_dir" == /tmp/notryx-credential-manual.* && -d "$test_dir" && ! -L "$test_dir" ]]; then
    rm -rf -- "$test_dir" || return 1
  fi
  [ ! -e "$test_dir" ] && [ ! -L "$test_dir" ]
}
trap 'test_status=$?; trap - EXIT; if ! cleanup; then printf "%s\n" "임시 시험 자료를 정리하지 못했습니다." >&2; exit 1; fi; exit "$test_status"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -m 700 -- "$test_dir/run" "$test_dir/tmp" "$test_dir/home" "$test_dir/xdg-data" "$test_dir/xdg-config" "$test_dir/xdg-cache" "$test_dir/control"
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
  *) printf '%s\n' '독립 D-Bus를 확인하지 못했습니다.' >&2; exit 1 ;;
esac
daemon_pid=
cleanup_daemon() {
  if [ -n "$daemon_pid" ]; then
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=
  fi
  owner=$(gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner org.freedesktop.secrets 2>/dev/null) || return 1
  [ "$owner" = '(false,)' ]
}
finish() {
  flow_status=$?
  trap - EXIT
  if ! cleanup_daemon; then
    printf '%s\n' '격리된 키링의 종료를 확인하지 못했습니다.' >&2
    exit 1
  fi
  exit "$flow_status"
}
trap finish EXIT
printf '%s\n' 'notryx-synthetic-keyring-password' |
  env LD_LIBRARY_PATH=/work/keyring-libs /work/keyring-daemon --foreground --unlock --components=secrets --control-directory=/work/control > /dev/null 2>&1 &
daemon_pid=$!
ready=0
for ((attempt = 0; attempt < 100; attempt++)); do
  if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner org.freedesktop.secrets 2>/dev/null | grep -Fxq '(true,)'; then ready=1; break; fi
  kill -0 "$daemon_pid" 2>/dev/null || break
  sleep 0.1
done
if [ "$ready" != 1 ]; then
  printf '%s\n' '격리된 키링을 시작하지 못했습니다.' >&2
  exit 1
fi
python3 /work/scripts/m3-credential-manual-flow.py --binary /work/notryx-client --server /work/notryx-server
INNER
chmod 700 "$test_dir/inner.sh"

test_result=0
env -i PATH=/usr/bin:/bin timeout --foreground --signal=TERM --kill-after=5s 1800s python3 -I -S -c '
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
  --die-with-parent --unshare-user --unshare-pid --unshare-ipc --unshare-net --uid 0 --gid 0 --cap-add CAP_NET_ADMIN \
  --ro-bind /usr /usr --ro-bind-try /bin /bin --ro-bind-try /lib /lib --ro-bind-try /lib64 /lib64 \
  --dir /etc --ro-bind /etc/passwd /etc/passwd --ro-bind /etc/group /etc/group \
  --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
  --ro-bind "$test_dir/machine-id" /etc/machine-id --bind "$test_dir" /work \
  --ro-bind "$test_socket" /work/run/wayland-0 \
  --ro-bind "$test_daemon" /work/keyring-daemon --ro-bind "$test_libraries" /work/keyring-libs \
  --ro-bind "$test_client" /work/notryx-client --ro-bind "$test_server" /work/notryx-server \
  --dir /work/scripts --ro-bind "$test_flow" /work/scripts/m3-credential-manual-flow.py \
  --proc /proc --dev /dev --tmpfs /tmp --tmpfs /run --tmpfs /home --dir /root \
  --chdir /work --clearenv --setenv PATH /usr/bin:/bin --setenv TMPDIR /work/tmp --setenv HOME /work/home \
  --setenv XDG_DATA_HOME /work/xdg-data --setenv XDG_CONFIG_HOME /work/xdg-config --setenv XDG_CACHE_HOME /work/xdg-cache --setenv XDG_RUNTIME_DIR /work/run \
  --setenv WAYLAND_DISPLAY wayland-0 --setenv GDK_BACKEND wayland --setenv SSL_CERT_FILE /work/ca-bundle --setenv NOTRYX_TEST_SANDBOX credential-manual \
  -- python3 -I -S -c '
import fcntl, socket, struct, subprocess
sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
request = struct.pack("16sH14x", b"lo", 0)
flags = struct.unpack("16sH14x", fcntl.ioctl(sock, 0x8913, request))[1]
fcntl.ioctl(sock, 0x8914, struct.pack("16sH14x", b"lo", flags | 1))
sock.close()
subprocess.run(["dbus-run-session", "--config-file=/work/bus.conf", "--", "/work/inner.sh"], check=True)
' || test_result=$?

if [ "$test_result" -ne 0 ]; then
  case "$test_result" in
    124) printf '%s\n' '시험 제한 시간이 지났습니다.' >&2 ;;
    137) printf '%s\n' '시험 프로세스가 종료됐습니다.' >&2 ;;
    *) printf '%s\n' '격리 시험이 완료되지 않았습니다.' >&2 ;;
  esac
  exit 1
fi
