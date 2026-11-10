#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ] && [ "$#" -ne 3 ]; then
  printf '%s\n' 'Usage: bash scripts/test-linux-deb-install.sh PACKAGE.deb [PREVIOUS_CLIENT SERVER]' >&2
  exit 2
fi
test_package=$(realpath -e -- "$1")
test_repo=$(realpath -e -- "$(dirname -- "$0")/..")
test_image=notryx-linux-package-test:local
docker build --tag "$test_image" - < "$test_repo/deploy/client-test.Dockerfile"
test_extra=()
test_upgrade=0
if [ "$#" -eq 3 ]; then
  test_previous=$(realpath -e -- "$2")
  test_server=$(realpath -e -- "$3")
  test_scripts=$(realpath -e -- "$(dirname -- "$0")")
  test_upgrade=1
  test_extra=(
    --security-opt seccomp=unconfined --security-opt apparmor=unconfined
    --tmpfs /work/tmp:rw,nosuid,nodev,size=256m,mode=1777
    --mount "type=bind,src=$test_previous,dst=/work/previous-client,readonly"
    --mount "type=bind,src=$test_server,dst=/work/notryx-server,readonly"
    --mount "type=bind,src=$test_scripts/m3-credential-restart-webdriver.py,dst=/work/scripts/m3-credential-restart-webdriver.py,readonly"
    --mount "type=bind,src=$test_scripts/m2-workspace-webdriver.py,dst=/work/scripts/m2-workspace-webdriver.py,readonly"
  )
fi

docker run --rm -i --memory=2g --cpus=1 \
  --mount "type=bind,src=$test_package,dst=/tmp/notryx.deb,readonly" \
  "${test_extra[@]}" --env "NOTRYX_TEST_UPGRADE=$test_upgrade" \
  "$test_image" bash -s <<'TEST'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

# A lower-version package exercises dpkg upgrade handling without publishing a fake release.
dpkg-deb -R /tmp/notryx.deb /tmp/previous
sed -i 's/^Version:.*/Version: 0.0.0~package-test/' /tmp/previous/DEBIAN/control
dpkg-deb --build --root-owner-group -Zgzip /tmp/previous /tmp/previous.deb
apt-get install --no-install-recommends -y /tmp/previous.deb desktop-file-utils
test "$(dpkg-query -W -f='${Status}' notryx)" = 'install ok installed'

mkdir -p /home/synthetic/.local/share/org.notryx.m0 /home/synthetic/.local/share/keyrings
printf '%s\n' 'synthetic draft sentinel' > /home/synthetic/.local/share/org.notryx.m0/preservation.txt
printf '%s\n' 'synthetic keyring sentinel' > /home/synthetic/.local/share/keyrings/preservation.txt
find /home/synthetic -type f -exec sha256sum {} + > /tmp/preservation.sha256

apt-get install --no-install-recommends -y /tmp/notryx.deb
test "$(dpkg-query -W -f='${Version}' notryx)" = "$(dpkg-deb -f /tmp/notryx.deb Version)"
test -z "$(dpkg --audit)"
desktop-file-validate /usr/share/applications/*.desktop
ldd /usr/bin/notryx-client > /tmp/linked-libraries
if grep -q 'not found' /tmp/linked-libraries; then
  cat /tmp/linked-libraries
  exit 1
fi
sha256sum --check /tmp/preservation.sha256
if [ "$NOTRYX_TEST_UPGRADE" = 1 ]; then
  mkdir -p /work/run /work/tmp /work/xdg-data /work/xdg-config /work/xdg-cache
  chmod 700 /work/run
  export TMPDIR=/work/tmp XDG_RUNTIME_DIR=/work/run
  export XDG_DATA_HOME=/work/xdg-data XDG_CONFIG_HOME=/work/xdg-config XDG_CACHE_HOME=/work/xdg-cache
  export NOTRYX_TEST_SANDBOX=credential-integration
  cat >/work/bus.conf <<'BUS'
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
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
  dbus-run-session --config-file=/work/bus.conf -- bash -c '
    set -euo pipefail
    printf "%s\n" notryx-synthetic-keyring-password |
      gnome-keyring-daemon --foreground --unlock --components=secrets --control-directory=/work/run/keyring >/work/keyring.log 2>&1 &
    daemon_pid=$!
    trap '\''kill "$daemon_pid" 2>/dev/null || true'\'' EXIT
    ready=0
    for attempt in $(seq 1 100); do
      if gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner org.freedesktop.secrets | grep -q true; then ready=1; break; fi
      sleep 0.1
    done
    test "$ready" = 1
    if ! timeout --kill-after=5s 300s xvfb-run -a python3 /work/scripts/m3-credential-restart-webdriver.py \
      --binary /work/previous-client --upgrade-binary /usr/bin/notryx-client --server /work/notryx-server; then
      printf "Scenario stage: "
      cat /work/scenario-stage
      exit 1
    fi
  '
fi
apt-get purge -y notryx
sha256sum --check /tmp/preservation.sha256
printf '%s\n' 'Linux package install, upgrade and removal passed; synthetic user files preserved.'
TEST
