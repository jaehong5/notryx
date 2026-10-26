# Notryx 개발 환경

이 문서는 Ubuntu 24.04 x86_64에서 Linux와 Android ARM64 개발에 사용하는 도구와 확인 명령을 정리한다. 현재 `0.1.0`의 클라이언트는 M0 편집 및 초안 저장 시험 앱이다. Linux 서버는 실행, 로컬 관리, 기기 등록과 인증된 조회를 제공하며 저장 코어의 조건부 본문 저장과 재시작 복구도 검사할 수 있다. 클라이언트 서버 연결과 동기화는 아직 없다.

## 버전과 설치 위치

2026-10-26에 확인한 도구 버전이다. 기존 Node.js와 Java는 유지했다.

| 도구 | 버전 | 위치 또는 고정 방법 |
|---|---|---|
| Rust | 1.99.1 | `~/.cargo`, `~/.rustup`, 저장소의 `rust-toolchain.toml` |
| Rust 구성 요소 | rustfmt, clippy, Android ARM64 표준 라이브러리 | `rust-toolchain.toml` |
| Node.js / npm | 24.22.1 / 11.21.0 | 기존 nvm 설치, `.nvmrc` |
| Tauri CLI | 2.12.4 | `~/.local/share/notryx/tooling`, npm lockfile |
| Java | OpenJDK 17.0.20.1 | 기존 시스템 JDK |
| Android Command-line Tools | 22.0 | `~/.local/share/notryx/android-sdk/cmdline-tools/latest` |
| Android SDK Platform | 37.0, revision 2 | SDK의 `platforms/android-37.0` |
| Tauri Android library의 SDK Platform | 36, revision 2 | 첫 APK 빌드에서 Gradle이 준비한 `platforms/android-36` |
| Android Build-Tools | 36.0.0 | SDK의 `build-tools/36.0.0` |
| Android Platform-Tools | 37.0.1 | SDK의 `platform-tools` |
| Android NDK | 30.0.16248370 | SDK의 `ndk/30.0.16248370` |

사용자 경로의 Tauri CLI는 초기 구성을 위한 도구다. 현재 앱은 프로젝트 manifest와 lockfile에 고정한 CLI, React, TypeScript, Vite와 테스트 도구를 사용한다. 주요 버전과 라이선스 metadata 범위는 [의존성 기록](dependencies.md)에 있다.

## 환경 불러오기

저장소 루트의 Bash에서 실행한다.

```bash
source "${NVM_DIR:-$HOME/.nvm}/nvm.sh"
nvm use
source scripts/dev-env.sh
```

환경 스크립트는 Rust와 Tauri CLI, Android 도구를 현재 셸의 PATH에 추가한다. 기존 `ANDROID_HOME`, `NDK_HOME`, `JAVA_HOME` 값이 있으면 유지한다. 값이 없으면 사용자 SDK 경로와 고정한 NDK를 사용하고 설치된 `javac`에서 JDK 위치를 찾는다. `.bashrc`와 `.profile`은 변경하지 않는다.

## Linux 시스템 패키지

[Tauri 2의 Linux 요구사항](https://v2.tauri.app/start/prerequisites/#linux)에 따라 WebKitGTK, GTK 관련 개발 라이브러리, OpenSSL과 빌드 도구를 설치한다. SQLite CLI와 화면 없는 환경의 기본 실행 점검을 위한 Xvfb도 포함한다.

`webkit2gtk-driver`도 설치 목록에 포함한다. 아래 스크립트로 설치한 환경에서는 별도 `--driver` 지정 없이 시험할 수 있다.

```bash
sudo bash scripts/install-linux-deps.sh
```

스크립트는 apt 패키지 목록을 갱신하고 필요한 패키지를 설치한다. 전체 시스템 업그레이드나 패키지 제거는 실행하지 않는다. 시스템 패키지 설치에는 sudo 인증이 필요하다.

## 사용자 도구 재설치

Rust 최초 설치는 [공식 rustup 안내](https://rust-lang.org/tools/install/)를 따른다. rustup이 설치된 뒤 저장소의 고정 버전을 준비한다.

```bash
rustup toolchain install 1.99.1 --profile minimal \
  --component rustfmt --component clippy \
  --target aarch64-linux-android
npm install --prefix "$HOME/.local/share/notryx/tooling" \
  --save-exact @tauri-apps/cli@2.12.4
```

Android Command-line Tools는 [공식 다운로드](https://developer.android.com/studio#command-tools)의 Linux 배포본을 받아 SHA-256을 대조하고 SDK 경로의 `cmdline-tools/latest`에 배치한다. 이번 설치는 배포 파일 `15859902`, SHA-256 `4e4c464f145a7512b57d088ac6c278c03c9eea610886b35a5e0804e74eedf583`을 사용했다. 환경을 불러온 뒤 설치할 패키지를 지정한다.

```bash
sdkmanager --sdk_root="$ANDROID_HOME" --licenses
sdkmanager --sdk_root="$ANDROID_HOME" \
  "platform-tools" "platforms;android-37.0" "platforms;android-36" \
  "build-tools;36.0.0" "ndk;30.0.16248370"
```

`platform-tools`와 SDK platform의 revision은 같은 패키지 이름에서도 바뀔 수 있으므로 재설치 후 실제 버전을 기록한다. `sdkmanager`는 현재 deprecated 경고를 내지만 설치와 조회는 정상 동작한다. 제공되는 새 Android CLI로 전환할 때는 명령과 설치 결과를 다시 검증한다.

SDK 37과 Build-Tools 36.0.0은 [Tauri CLI 2.12.4의 Android 템플릿](https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.12.4/crates/tauri-cli/templates/mobile/android/app/build.gradle.kts) 및 [AGP 9.3 호환성 표](https://developer.android.com/build/releases/agp-9-3-0-release-notes)를 기준으로 선택했다. NDK는 [공식 LTS 배포](https://developer.android.com/ndk/downloads)를 사용한다. Android Studio, 에뮬레이터와 추가 ABI는 실제 시험에 필요할 때 설치한다.

## 환경 확인

```bash
source scripts/dev-env.sh
rustc --version
cargo --version
rustup component list --installed
rustup target list --installed
node --version
npm --version
tauri --version
javac -version
sdkmanager --sdk_root="$ANDROID_HOME" --list_installed
adb version
pkg-config --modversion gtk+-3.0 webkit2gtk-4.1 libsoup-3.0 openssl
sqlite3 --version
```

도구 설치와 앱 빌드는 실제 기기의 입력, 접근성, 저장 성능과 복구 검증을 대신하지 않는다. 측정 전에 장치, workload, 허용 지연과 자원 기준을 정하고 실제 실행 결과와 미실행 항목을 구분한다.

## 프로젝트 실행과 검증

```bash
npm ci
npm run tauri -- dev
```

원문 편집은 새 문서와 실제 편집한 checkpoint의 줄바꿈을 LF로 저장한다. 수정하지 않은 문서는 원래 bytes를 유지한다. CRLF, 혼합 줄바꿈과 단독 CR은 유효 UTF-8이고 크기 상한 이내이면 안내 후 편집할 수 있다. 저장 후 WebView를 재로드하면 완료 checkpoint와 generation을 다시 열어 편집할 수 있어야 하며, base는 유지해야 한다. 구현 및 시험 결과는 이 기대 결과와 별도로 확인한다.

웹 개발 서버는 `127.0.0.1:1420`에서 실행한다. 브라우저 단독 실행은 native 초안 저장을 제공하지 않으므로 실제 편집 시험에는 Tauri 앱을 사용한다.

```bash
bash scripts/check.sh
npm run tauri -- build --debug --no-bundle
xvfb-run -a python3 scripts/m0-webdriver.py
xvfb-run -a python3 scripts/m0-webdriver.py --measure
cargo run --locked -p notryx-client-store --example checkpoint_bench
python3 scripts/m0-fsync.py
npm run tauri -- android build --debug --target aarch64 --apk --ci
```

WebKitWebDriver를 별도 위치에 추출했다면 `--driver /path/to/WebKitWebDriver`를 추가한다. UI와 저장 성능 측정 중에는 다른 빌드를 실행하지 않는다. 현재 `.cargo/config.toml`은 작업 수를 2로 제한하며 Android도 Gradle worker 2개와 최대 JVM heap 2 GiB를 사용한다. Android Studio와 에뮬레이터는 사용하지 않는다.

전체 검사는 TS build/lint/unit, Rust format/clippy/unit/integration, 생성 계약 drift, 순수 계층 의존성과 npm 보안 정보를 확인한다. 저장 시험은 독립 임시 DB를 만들고 실제 프로세스 강제 종료도 수행한다. WebDriver 역시 임시 client store를 사용하며 결과와 화면을 `.artifacts/m0-webview`에 남긴다. 성능은 검사 프로세스 종료 코드와 구분해 측정 전에 정한 기준에 따라 판정한다.

`.github/workflows/check.yml`은 같은 Linux 검사를 CI에 연결한다. 원격 GitHub Actions 실행은 소스를 올린 뒤 별도로 확인해야 한다. 프로토콜 파일은 `cargo run -p notryx-protocol --bin generate-contracts`로 생성하고 직접 수정하지 않는다.


## 개발 서버 실행과 조회

아래 예제는 새 합성 저장소와 loopback HTTP를 사용한다. Linux `openat2`가 필요하며 지원하지 않는 환경에서는 시작을 거절한다. 서버의 전용 worker가 Workspace와 파일 I/O를 소유한다. 초기화, migration과 파일 등록은 서버를 중지한 상태에서 실행한다.

### 새 저장소와 실행

아직 사용하지 않은 `/tmp/notryx-dev` 경로를 사용한다. notes와 state는 서로 다른 빈 디렉터리여야 한다. 설정의 저장소와 관리자 socket 경로는 절대 경로다.

```bash
source scripts/dev-env.sh
cargo build --locked -p notryx-server
umask 077
mkdir -m 0700 /tmp/notryx-dev
mkdir -m 0700 /tmp/notryx-dev/notes /tmp/notryx-dev/state /tmp/notryx-dev/run
cat > /tmp/notryx-dev/server.toml <<'TOML'
[server]
listen = "127.0.0.1:8080"
admin_socket = "/tmp/notryx-dev/run/admin.sock"

[workspace]
notes_root = "/tmp/notryx-dev/notes"
state_root = "/tmp/notryx-dev/state"
TOML
target/debug/notryx-server init --config /tmp/notryx-dev/server.toml
cat > /tmp/notryx-dev/notes/sample.md <<'MARKDOWN'
# Development

Synthetic note.
MARKDOWN
target/debug/notryx-server adopt --config /tmp/notryx-dev/server.toml sample.md
target/debug/notryx-server serve --config /tmp/notryx-dev/server.toml
```

`init`는 새 schema 2 저장소와 credential이 없는 최초 owner를 만든다. `adopt`는 notes 루트 바로 아래의 기존 일반 파일 하나를 등록하고 `node_id`를 JSON으로 출력한다. 경로나 디렉터리 대신 파일 이름을 전달한다. 아래 조회 예제에 이 ID를 사용한다. 전체 폴더 가져오기와 외부 변경 자동 수용은 제공하지 않는다.

`serve`는 기존 저장소를 열고 복구한 뒤 요청을 처리한다. 누락되거나 손상된 저장소를 초기화하지 않는다. `Ctrl+C` 또는 SIGTERM으로 새 작업 접수를 닫고 진행 작업을 마친 뒤 잠금을 해제한다. 다른 터미널에서 health를 확인한다.

```bash
curl --fail http://127.0.0.1:8080/health/live
curl --fail http://127.0.0.1:8080/health/ready
```

`live`는 HTTP 응답 여부, `ready`는 복구 후 저장소 준비 상태를 나타낸다. 준비되지 않았거나 worker에 요청할 수 없으면 `ready`는 503이다. 이 서버의 HTTP 예제는 로컬 개발용이며 Docker 및 운영 배포 절차는 아직 제공하지 않는다.

### 기존 schema 1 저장소

기존 개발 저장소가 `MigrationRequired` 오류로 기동 거절되면 서버를 중지하고 해당 저장소의 설정으로 명시적으로 실행한다. `init`를 다시 실행하지 않는다.

```bash
target/debug/notryx-server migrate --config /tmp/notryx-dev/server.toml
```

migration은 notes/state 잠금과 기존 무결성을 검사하고 schema 1에서 2로 인증 테이블과 최초 owner를 추가한다. 문서, revision, epoch, stream, receipt와 intent를 유지하며 자동 migration은 하지 않는다. 변경은 한 transaction으로 처리하고 실패 시 기존 schema를 유지한다.

### 로컬 관리자와 기기 등록

관리자 CLI는 실행 중인 서버의 Unix socket을 사용하며 DB를 직접 수정하지 않는다. socket 부모 디렉터리는 서버 사용자 소유의 `0700` 디렉터리로 준비한다. socket은 `0600`이고 양쪽이 같은 UID인지 확인한다. 기존 일반 파일이나 symlink를 socket으로 덮어쓰지 않는다.

서버는 socket 경로 뒤에 `.lock`을 붙인 `0600` 파일을 잠근다. 오래된 socket의 회수부터 종료 시 socket 정리까지 잠금을 유지하며, 같은 socket을 쓰는 다른 서버의 시작을 거절한다. 잠금 파일은 종료 후에도 남으며 다음 실행에서 재사용한다. 실행 중 이 파일을 삭제하거나 교체하지 않는다.

```bash
target/debug/notryx-server admin --config /tmp/notryx-dev/server.toml status
target/debug/notryx-server admin --config /tmp/notryx-dev/server.toml create-principal reader
```

`status`에서 workspace 및 owner의 ID를, `create-principal`에서 새 `principal_id`를 확인한다. 읽기 시험은 새 reader로 진행할 수 있다. 다음 `<principal UUID>`를 실제 ID로 바꿔 실행한다.

```text
target/debug/notryx-server admin --config /tmp/notryx-dev/server.toml issue-pairing <principal UUID>
```

`issue-pairing`은 사용자가 요청한 표준 출력에 `workspace_id`, `pairing_secret`, `expires_at`을 포함한 JSON을 출력한다. 이 출력은 credential을 포함하므로 로그에 남기지 않는다. secret과 token은 명령행 인자, URL 또는 로그에 넣지 않는다. pairing은 5분 동안 한 번만 소비할 수 있다. 응답이 유실되면 같은 secret으로 token을 다시 발급받을 수 없으므로 새 pairing을 발급한다.

다른 관리자 명령은 다음과 같다. 명령과 인자 목록은 `target/debug/notryx-server --help`에서도 확인할 수 있다.

| `admin --config <설정 파일>` 뒤의 명령 | 동작 |
|---|---|
| `create-principal <reader\|editor>` | principal 생성 |
| `set-role <principal UUID> <reader\|editor>` | owner가 아닌 principal의 역할 변경 |
| `transfer-owner <principal UUID> <이전 owner의 reader\|editor 역할>` | 기존 principal로 owner 이전 |
| `revoke-device <device UUID>` | 기기 token 폐기 |

### 인증된 HTTP 조회

HTTP 계약은 [Rust DTO](../crates/protocol/src/http.rs), 실제 경로와 header 처리는 [HTTP adapter](../apps/server/src/http.rs)에 있다. protocol은 `mdw-sync/1.2`다. pairing의 JSON 요청 상한은 1 MiB이고, 문서 본문 조회는 최대 5 MiB의 raw bytes를 반환한다.

| 경로 | 요청과 결과 |
|---|---|
| `POST /v1/pair` | `protocol`, `workspace_id`, `pairing_secret` JSON. 기기 token과 새 stream, 만료 시각 반환 |
| `GET /v1/server-info` | Bearer 인증. workspace, epoch, root, head와 제한 조회 |
| `GET /v1/nodes/{id}/metadata` | Bearer 인증. node metadata와 metadata 전용 ETag 반환 |
| `GET /v1/nodes/{id}/content` | Bearer 인증. commit된 원문 bytes, content 전용 ETag와 `Content-Digest` 반환 |

본문의 BOM과 CRLF도 그대로 반환한다. content의 선택적 `If-Match`에는 이전 content 응답의 strong ETag 하나를 그대로 사용한다. 잘못된 형식은 400, 현재 content와 불일치하면 412의 `content_revision_mismatch`다. metadata ETag를 content 조건으로 사용하지 않는다. 미인증, 만료되거나 폐기된 token은 401이다. API 오류는 `code`, 정적인 사용자 메시지와 UUIDv7 `request_id`를 반환하며 요청 본문이나 credential을 포함하지 않는다.

아래 Python 예제는 pairing secret을 화면에 표시하지 않고 읽으며 발급받은 token을 프로세스 메모리에만 둔다. workspace ID는 `status` 또는 `issue-pairing`, node ID는 `adopt` 출력에서 가져온다. 앞서 실행한 `serve`를 유지한 채 다른 터미널에서 실행한다. 출력에는 token과 본문을 포함하지 않는다.

```bash
python3 -c '
import base64
import getpass
import hashlib
import json
import urllib.request

origin = "http://127.0.0.1:8080"
workspace_id = input("Workspace UUID: ").strip()
node_id = input("Node UUID: ").strip()
secret = getpass.getpass("Pairing secret: ")
request = urllib.request.Request(
    origin + "/v1/pair",
    data=json.dumps({"protocol": "mdw-sync/1.2", "workspace_id": workspace_id,
                     "pairing_secret": secret}).encode(),
    headers={"Content-Type": "application/json"},
)
with urllib.request.urlopen(request, timeout=5) as response:
    paired = json.load(response)
headers = {"Authorization": "Bearer " + paired["device_token"]}
print("Device UUID:", paired["device_id"])
for path in ("/v1/server-info", "/v1/nodes/" + node_id + "/metadata"):
    with urllib.request.urlopen(urllib.request.Request(origin + path, headers=headers), timeout=5) as response:
        print(json.load(response))
request = urllib.request.Request(origin + "/v1/nodes/" + node_id + "/content", headers=headers)
with urllib.request.urlopen(request, timeout=5) as response:
    body = response.read()
    digest = "sha-256=:" + base64.b64encode(hashlib.sha256(body).digest()).decode() + ":"
    assert response.headers["Content-Digest"] == digest
    print("Content bytes:", len(body), "ETag:", response.headers["ETag"])
'
```

기기 token은 발급 시 idle 90일, absolute 365일 만료가 설정된다. 현재 조회 요청은 idle 만료를 연장하지 않으며 token renew도 아직 없다. 예제를 종료하면 token을 다시 사용할 수 있도록 저장하지 않는다. 필요하면 출력된 기기 ID로 `revoke-device`를 실행하고 새 pairing으로 시험한다.

### 서버 검사와 남은 범위

```bash
cargo test --locked -p notryx-server -p notryx-server-storage -p notryx-protocol
```

검사는 임시 합성 저장소에서 localhost HTTP, Unix socket, CLI, 등록과 조회 및 재시작을 다룬다. 저장 코어 검사는 admission과 최종 commit의 SQL 실패, 파일 및 디렉터리 영속화 오류 주입, 별도 프로세스의 SIGKILL과 재시작을 포함한다. 자식 프로세스용 ignored 테스트는 부모 테스트가 직접 실행한다. failpoint 도달 대기에는 timeout을 두고 timeout 또는 조기 종료를 실패로 처리하며, 자식 프로세스는 종료 후 wait하여 회수한다. 이 검사 명령의 제공은 실제 전원 장애 시험이나 M1 전체 통과를 뜻하지 않는다.

저장 코어는 content revision, 요청 순번과 digest를 검사한다. 저장 전후의 예상 상태와 다른 파일이 발견되면 자료를 보존하고 쓰기를 막는다. 복구 충돌의 관리자 해결 기능은 아직 없으며, 다른 프로세스의 동시 파일 변경을 완전히 제어할 수는 없다.

원격 본문 쓰기, 폴더 CRUD, upload, operations 조회, receipt ACK와 GC, SSE, token renew, gateway 인증, Docker와 클라이언트 서버 연동은 후속 범위다. 실제 Linux IME와 Android 실기기 검증도 별도로 남아 있다. 실패 주입 hook은 테스트용이며 운영 환경변수로 활성화하지 않는다.
