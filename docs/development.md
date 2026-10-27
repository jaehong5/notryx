# Notryx 개발 환경

이 문서는 Ubuntu 24.04 x86_64에서 Linux와 Android ARM64 개발에 사용하는 도구와 확인 명령을 정리한다. 현재 `0.1.0`의 클라이언트는 M0 편집 및 초안 저장 시험 앱이다. Linux 서버는 실행, 로컬 관리, 기기 등록과 인증된 파일 및 폴더 관리를 제공한다. 원문 조회와 조건부 저장, 이름 변경과 이동, 삭제와 복원 및 기기별 작업 결과 관리를 지원한다. 클라이언트 서버 연결과 동기화는 아직 없다.

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

`.github/workflows/check.yml`은 Linux 검사와 Tauri WebDriver 시험, 서버 Docker 이미지 빌드와 격리 실행 시험을 CI에 연결한다. 생성된 WebDriver 실패 자료와 Docker 시험 결과는 각각 7일간 보관한다. CI는 이미지를 게시하거나 서버에 배포하지 않는다. 원격 GitHub Actions 실행은 소스를 올린 뒤 별도로 확인해야 한다. 프로토콜 파일은 `cargo run -p notryx-protocol --bin generate-contracts`로 생성하고 직접 수정하지 않는다.


## 개발 서버 실행과 조회

아래 예제는 새 합성 저장소와 loopback HTTP를 사용한다. Linux `openat2`가 필요하며 지원하지 않는 환경에서는 시작을 거절한다. 서버의 전용 worker가 Workspace와 파일 I/O를 소유한다. 초기화, migration과 파일 등록은 서버를 중지한 상태에서 실행한다.

파일 게시에는 로컬 파일시스템의 `O_TMPFILE` 지원도 필요하다. 새 파일은 이름 없는 파일을 완성한 뒤 최종 이름으로 연결한다. 지원하지 않는 환경에서 일반 임시 파일로 우회하지 않는다. 실제 배포 파일시스템에서 저장과 재시작 시험을 실행해야 한다.

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

`init`는 새 schema 9 저장소와 credential이 없는 최초 owner를 만든다. `adopt`는 notes 루트 바로 아래의 기존 일반 파일 하나를 등록하고 `node_id`를 JSON으로 출력한다. 경로나 디렉터리 대신 파일 이름을 전달한다. 아래 조회 예제에 이 ID를 사용한다. 전체 폴더 가져오기와 외부 변경 자동 수용은 제공하지 않는다.

`serve`는 기존 저장소를 열고 복구한 뒤 요청을 처리한다. 누락되거나 손상된 저장소를 초기화하지 않는다. `Ctrl+C` 또는 SIGTERM으로 새 작업 접수를 닫고 진행 작업을 마친 뒤 잠금을 해제한다. 다른 터미널에서 health를 확인한다.

```bash
curl --fail http://127.0.0.1:8080/health/live
curl --fail http://127.0.0.1:8080/health/ready
```

`live`는 HTTP 응답 여부, `ready`는 복구 후 저장소 준비 상태를 나타낸다. 준비되지 않았거나 worker에 요청할 수 없으면 `ready`는 503이다. 이 HTTP 예제는 로컬 개발용이다. 서버 이미지 빌드와 격리 실행은 [Docker 안내](deployment.md)를 따른다.

### 기존 schema 1~8 저장소

기존 개발 저장소가 `MigrationRequired` 오류로 기동 거절되면 서버를 중지하고 해당 저장소의 설정으로 명시적으로 실행한다. `init`를 다시 실행하지 않는다.

```bash
target/debug/notryx-server migrate --config /tmp/notryx-dev/server.toml
```

migration은 notes/state 잠금과 기존 무결성을 검사하고 schema 1~8을 9로 전환한다. schema 1에는 인증 테이블과 최초 owner를 추가하고, schema 1/2에는 receipt 회수 watermark와 최소 식별 기록을 추가한다. schema 4는 하위 폴더와 생성 intent를 지원하는 저장 형식을 구분한다. schema 5는 성공 receipt의 node ETag를 보존하는 컬럼을 추가하고 기존 계산값을 기록한다. schema 6은 이동 intent와 누적 자식 namespace 검사를 구분한다. schema 7은 tombstone과 살아 있는 node만의 이름 유일성을 지원하도록 nodes 테이블을 교체한다. schema 8은 receipt ACK와 독립적으로 유지하는 격리 사건과 보존 객체 참조를 추가한다. schema 9는 기기별 임시 업로드의 예약, 만료와 pending intent 참조를 추가한다. 문서, revision, epoch, 기존 owner와 token, stream, receipt, 회수 기록과 pending intent를 보존하며 자동 migration은 하지 않는다. foreign key 검사를 잠시 끈 상태에서 테이블을 교체하되 데이터 복사, 무결성 검사와 버전 전환은 한 transaction으로 처리한다. 실패 시 기존 schema를 유지하고 foreign key 검사를 다시 켠다. 현재 schema에서 다시 실행해도 owner나 stream을 새로 만들지 않는다.

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

### 복구 충돌 진단과 명시적 해결

복구가 before 또는 proposed로 판정되지 않으면 서버는 자료를 보존하고 신규 쓰기를 중지한다. 인증된 읽기는 마지막 commit 상태를 제공하며 `server-info.ready`는 false다. 다음 명령은 같은 UID의 로컬 관리자 socket에서만 제공한다.

| `admin --config <설정 파일>` 뒤의 명령 | 동작 |
|---|---|
| `recovery-list [이전 페이지 마지막 operation UUID]` | 최대 20개 사건과 다음 cursor 조회 |
| `recovery-inspect <operation UUID> [after SHA-256 hex]` | 관련 경로와 보존 객체를 최대 64개씩 조회 |
| `recovery-resolve <operation UUID>` | 현재 파일 상태를 검증하고 명시적으로 채택 |
| `recovery-release <operation UUID>` | 해결한 사건의 보존 참조를 명시적으로 해제 |

외부 writer를 중지하고 현재 파일과 보존 자료를 대조한 뒤 `recovery-resolve`를 실행한다. 구조가 모호하거나 위험한 링크와 이름 충돌이 있으면 파일을 별도로 정리한 후 다시 검사한다. 서버는 과거 bytes로 파일을 덮어쓰거나 이동 여부를 추정해 ID를 합치지 않는다.

진단의 `objects`는 `sha256:` 접두어를 붙인 hash이며 본문은 출력하지 않는다. 실제 사본은 state 디렉터리의 `objects/sha256/<64자리 hash>`에 있다. `next_after_hash`가 null이 아니면 그 값을 다음 `recovery-inspect`의 마지막 인자로 전달한다. `retained_objects`는 전체 개수이고 `objects_truncated`는 다음 페이지가 있다는 뜻이다.

해결은 기존 작업을 terminal `recovery_conflict`로 종료한다. 과거 적용 여부를 확정한 성공 또는 미적용 결과가 아니다. 원래 요청의 재시도는 같은 409 결과를 반환한다. 클라이언트는 현재 문서와 초안을 대조하고 새 조건부 작업으로 제출해야 한다. 모든 pending과 충돌을 해결하고 정합성 검사를 통과하면 readiness를 재개한다.

해결 명령은 응답을 최대 120초 기다린다. 응답 유실이나 timeout은 작업 취소를 뜻하지 않는다. 같은 operation UUID로 다시 요청하거나 사건 상태를 조회한다. 일반 관리자 명령과 frame 입출력 제한은 5초다. receipt ACK는 보존 사본을 해제하지 않는다. `recovery-release`는 해결된 사건에만 허용하며 객체 파일의 즉시 삭제를 뜻하지 않는다.

본문 교체 중 사용하는 임시 파일의 소유권 기록은 state의 `temporary` 디렉터리에 저장한다. 시작 복구는 부모와 파일의 식별 정보 및 본문이 기록과 일치하는 임시 항목만 정리한다. 파일이 이미 없어도 부모 디렉터리를 먼저 영속화한 뒤 기록을 제거한다. 이 디렉터리는 필요할 때 생성하며 SQLite schema와 wire 계약은 바꾸지 않는다.

기록이 없는 이전 버전의 `.mdw-tmp-*` 파일, 외부에서 수정하거나 교체한 항목은 자동 삭제하지 않는다. 이런 항목 때문에 폴더 삭제가 거절되면 외부 writer를 중지하고 자료를 보존한 채 별도로 대조해야 한다. 이름 접두어만 보고 일괄 삭제하지 않는다.

### 인증된 HTTP 조회

HTTP 계약은 [Rust DTO](../crates/protocol/src/http.rs), 실제 경로와 header 처리는 [HTTP adapter](../apps/server/src/http.rs)에 있다. protocol은 `mdw-sync/1.2`다. pairing의 JSON 요청 상한은 1 MiB이고, 본문 조회는 최대 25 MiB의 raw bytes를 반환한다. 편집기의 문서 상한은 5 MiB다.

| 경로 | 요청과 결과 |
|---|---|
| `POST /v1/pair` | `protocol`, `workspace_id`, `pairing_secret` JSON. 기기 token과 새 stream, 만료 시각 반환 |
| `GET /v1/server-info` | Bearer 인증. workspace, epoch, root, head와 제한 조회 |
| `GET /v1/nodes?after_id={id}&limit={n}` | Bearer 인증. root를 포함한 살아 있는 node의 UUID keyset 페이지 |
| `GET /v1/nodes/{id}` | Bearer 인증. 전체 own-state와 node 전용 ETag 반환 |
| `GET /v1/nodes/{id}/metadata` | Bearer 인증. node metadata와 metadata 전용 ETag 반환 |
| `GET /v1/nodes/{id}/content` | Bearer 인증. commit된 원문 bytes, content 전용 ETag와 `Content-Digest` 반환 |
| `GET /v1/trash?after_id={id}&limit={n}` | Bearer 인증. 삭제된 node의 UUID keyset 페이지와 복원 가능 여부 |

목록은 UUID 바이트 순서이며 `after_id`보다 큰 ID부터 반환한다. cursor ID가 실제로 존재할 필요는 없다. `limit`는 기본 100, 최소 1, 최대 100이며 전체 JSON 응답은 1 MiB 이하다. 응답 필드는 `workspace_epoch`, `nodes`, `next_after_id`다. 다음 페이지가 있으면 마지막 반환 ID를 cursor로 제공하고 끝이면 null이다. 각 페이지는 별도 read transaction이므로 목록 전체를 고정 snapshot으로 취급하지 않는다. 중복 또는 미지정 query와 잘못된 UUID 및 limit는 400이다.

전체 node 조회는 본문 bytes와 파생 경로를 포함하지 않는다. root는 빈 이름과 null parent를 가진 폴더다. node ETag는 본문 상태, 구조와 직접 자식 namespace 및 마지막 변경 순번을 반영한다. 새 생성, 저장, 이동, 삭제와 복원 receipt의 node ETag도 같은 계산을 사용한다. 구 schema에서 migration한 Saved receipt는 과거 ETag를 유지하므로 새 전체 조회와 다를 수 있다. 과거 작업 결과를 고치지 말고 현재 조건부 작업에는 새 조회에서 받은 validator를 사용한다. metadata 조회는 CAS 본문 유실이나 복구 대기 중에도 마지막 commit 상태를 반환한다.

전체 node와 metadata 조회는 삭제된 node도 반환하지만 content 조회는 410 `node_deleted`다. live 목록에는 삭제된 node가 없다. 휴지통 목록은 같은 페이지 규칙을 사용하고 reader도 조회할 수 있다. 휴지통 항목의 `restorable`은 조회 시점의 안내다. 복원 요청에서 본문 보존 상태를 다시 검사한다.

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

### 파일과 폴더 생성

`POST /v1/nodes`는 editor 또는 owner가 파일이나 폴더를 만든다. 요청은 `Content-Type: application/json`, 최대 1 MiB이며 아래 필드만 받는다.

| JSON 필드 | 값 |
|---|---|
| `node_id` | 요청 전에 생성하고 보존한 새 UUIDv7 |
| `parent_id` | 현재 존재하는 부모 폴더의 UUIDv7. 루트 ID는 server-info에서 조회 |
| `name` | NFC 이름 한 segment. `/`, `\\`, 예약 이름과 끝의 점 또는 공백은 거절 |
| `kind` | `file` 또는 `folder` |
| `upload_id` | 선택 사항. 같은 기기의 완료된 업로드 UUIDv4. 파일에만 사용 |

인증 및 작업 식별 헤더는 아래 PUT과 같은 `Authorization`, `X-Workspace-Epoch`, `X-Device-Stream`, `X-Operation-Seq`, `Idempotency-Key`다. 생성에는 `If-Match`와 `Content-Digest`를 요구하지 않는다. 파일은 `upload_id`를 생략하면 0 bytes로 만들고, 지정하면 같은 기기의 완료된 업로드 원문으로 만든다. 폴더에는 upload를 지정할 수 없다.

성공은 200과 `created` receipt다. 생성 당시 node의 전체 own-state, ETag와 commit 순번을 반환한다. 폴더의 본문 revision, ETag, hash와 size는 null이다. 생성된 폴더 안에 다시 파일이나 폴더를 만들 수 있으며, 기존 metadata/content 조회와 조건부 PUT도 ID로 하위 파일에 접근한다. 경로 상한은 1,024 UTF-8 bytes다. 파일이나 폴더 생성으로 부모의 meta ETag는 바뀌지 않는다.

서버는 기존 ID 재사용과 이름 충돌을 거절한다. 실제 디렉터리의 미추적 항목도 NFC/case-fold 충돌 검사에 포함하며 대상을 덮어쓰지 않는다. 목적지 충돌은 409 `destination_exists`, ID 충돌은 409 `node_id_conflict`, 파일을 부모로 지정하면 409 `parent_not_folder`, 없는 부모는 404다. 접수된 확정 실패는 receipt로 보존한다. 이름과 입력 형식, 인증 오류 등 접수 전 실패는 순번을 소비하지 않는다.

응답이 유실되면 같은 ID, 순번과 JSON으로 재시도하거나 자기 operation을 조회한다. 생성 뒤 본문을 수정했어도 과거 생성 재시도는 원래 결과를 반환하고 현재 본문을 비우지 않는다. ACK 이후에는 같은 생성의 재실행을 410으로 차단한다. reader 강등 뒤에도 기존 결과 조회와 동일 요청 재시도는 가능하지만 신규 생성은 거절한다.

### 첨부 업로드

`POST /v1/uploads`는 Bearer 인증과 `Content-Digest: sha-256=:BASE64:`를 사용한다. `Content-Encoding`은 생략하거나 `identity` 하나만 지정한다. 서버는 최대 25 MiB를 스트리밍으로 수신하고 정확한 bytes를 검증한다. 이 요청은 문서 생성이나 변경 순번을 소비하지 않으며 reader는 업로드할 수 없다.

성공은 201이며 `upload_id`, `content_hash`, `size`, `expires_at`를 반환한다. hash는 `sha256:` 뒤의 소문자 hex이고, size와 Unix 초 단위 만료 시각은 십진 문자열이다. 완료한 업로드의 유효 기간은 24시간이다. 수신은 최대 120초이며 5초 구간마다 최소 5 KiB의 진행을 요구한다. 첫 구간 전에 끝나는 작은 업로드는 허용한다.

다음 설정은 기본값이며 `[workspace]`의 하위 table로 추가할 수 있다. 개수 제한에는 빈 업로드와 수신 중 예약도 포함한다. `Content-Length`가 있으면 선언한 크기를 예약하고 정확한 일치를 요구하며, 없으면 파일 상한을 예약한 뒤 완료한 크기로 조정한다.

```toml
[workspace.uploads]
max_device_bytes = 134217728
max_device_uploads = 64
max_workspace_bytes = 536870912
max_workspace_uploads = 256
max_receiving = 2
```

이 quota는 임시 업로드에만 적용한다. 기기별 한도는 workspace 한도 이하여야 한다. workspace 최대 설정값은 64 GiB와 4,096개, 동시 수신은 1~16개이며 workspace 개수 상한 이하여야 한다. byte 및 개수 상한은 양수다. 보관 한도 초과는 507 `upload_quota`, 실제 저장 공간 부족은 507 `insufficient_storage`, 동시 수신 제한은 429 `busy`, 파일 상한 초과는 413이다. body의 digest 불일치는 400 `digest_mismatch`다.

완료한 `upload_id`를 파일 생성에 지정하면 성공 transaction에서 한 번 소비한다. 같은 생성 작업의 재시도는 upload 소비나 만료 후에도 원래 receipt를 반환한다. 다른 기기의 upload는 403, 만료되거나 소비된 upload의 신규 연결은 410 `upload_unavailable`이다. 접수 전 실패나 terminal 실패는 성공 연결이 아니므로 유효 기간 안에서 새 작업으로 다시 연결할 수 있다. pending 생성이 참조하는 upload는 만료로 정리하지 않는다.

서버 시작, 새 업로드 접수, 취소와 생성 성공 뒤에 알려진 staging 임시물과 만료된 업로드를 정리한다. 이 정리는 일반 본문 객체나 격리 보존 자료를 삭제하지 않는다. 업로드 응답을 잃으면 다시 업로드할 수 있으며 미연결 자료는 quota와 만료로 제한한다. 문서 생성 요청은 기존 operation ID로 재시도해야 한다.

### 이름 변경, 이동, 삭제와 복원

아래 요청은 생성과 같은 인증 및 작업 식별 헤더를 사용한다. 신규 작업은 editor 또는 owner만 가능하다. 동일 작업 재시도는 현재 인증과 stream을 확인한 뒤 저장된 결과를 반환하며, reader로 강등돼도 자기 기존 결과를 재현할 수 있다. operation ID나 순번을 재사용하면서 조건, 대상 또는 요청 필드를 바꾸면 409 `idempotency_mismatch`다.

| 경로 | 조건과 본문 | 성공 status |
|---|---|---|
| `PATCH /v1/nodes/{id}/metadata` | `If-Match`에 meta ETag. JSON은 `expected_parent_id`, `parent_id`, `name` | `moved` |
| `DELETE /v1/nodes/{id}` | `If-Match`에 전체 node ETag. 본문 없음 | `deleted` |
| `POST /v1/nodes/{id}/restore` | JSON은 `expected_meta_rev`, `parent_id`, `name` | `restored` |

JSON은 `Content-Type: application/json`이며 상한은 1 MiB다. 필수 조건 헤더 누락은 428, 약한 ETag와 여러 ETag, 중복 헤더 및 미지정 JSON 필드는 400이다. 조건은 조회한 값을 그대로 사용한다. ETag의 접두사나 내용을 해석해서 만들지 않는다. 복원의 `expected_meta_rev`는 휴지통 또는 전체 node 조회의 삭제된 revision이며 빈 값과 256 UTF-8 bytes 초과를 거절한다.

이동은 node의 meta revision만 바꾼다. 직접 부모의 children_generation은 부모별 한 번 증가하고 부모의 meta revision은 유지한다. 폴더 후손의 revision은 바뀌지 않는다. 같은 부모와 이름 요청은 조건과 실제 파일 상태를 확인한 뒤 상태 변경 없이 receipt만 기록한다. 대소문자만 바꾸는 이름 변경도 지원한다. 원문과 Markdown 링크를 재작성하지 않으며 링크 영향 표시는 아직 없다.

삭제는 전체 own-state를 검사하고 tombstone을 남긴다. 파일의 content revision과 모든 node의 meta revision을 새로 발급한다. root는 삭제하거나 이동할 수 없고 폴더는 실제 디렉터리가 비어 있어야 한다. 미추적 파일이나 숨김 파일도 자식으로 취급하며 재귀 삭제는 제공하지 않는다. 삭제한 이름은 새 ID로 재사용할 수 있지만 이전 ID는 새 생성에 사용할 수 없다.

복원은 기존 ID와 kind를 유지하고 새 meta revision 및 파일의 새 content revision을 발급한다. 파일은 보관한 bytes를 복원하며 BOM, 줄바꿈과 비UTF-8 bytes를 변환하지 않는다. 폴더는 이전 children_generation을 유지한 빈 폴더로 복원한다. 자식은 자동 복원하지 않는다. 다른 살아 있는 부모와 이름으로 복원할 수 있다. 보관 본문이 만료된 경우에도 tombstone 조회는 가능하다.

| 거절 원인 | HTTP와 code |
|---|---|
| metadata 조건 불일치 | 412 `metadata_revision_mismatch` |
| node 조건 불일치 | 412 `node_revision_mismatch` |
| 기대한 기존 부모 불일치 | 409 `expected_parent_mismatch` |
| 폴더의 자기 자신 또는 후손으로 이동 | 409 `cycle_detected` |
| root 변경 | 409 `root_immutable` |
| 목적지 이름 충돌 | 409 `destination_exists` |
| 비어 있지 않은 폴더 삭제 | 409 `folder_not_empty` |
| 살아 있는 node 복원 | 409 `not_deleted` |
| 삭제된 대상의 저장, 이동, 삭제 또는 삭제된 부모 지정 | 410 `node_deleted` |
| 복원 원문 만료 | 410 `history_expired` |

성공은 200과 변경 당시 전체 node 및 commit 순번이다. 접수된 확정 실패도 receipt로 보존한다. 재시도는 이후 삭제나 복원을 되돌리지 않는다. 예를 들어 복원 성공 뒤 다시 삭제했더라도 이전 복원을 재전송하면 당시 결과만 반환하며 파일을 되살리지 않는다. 일반 객체 GC와 휴지통 영구 삭제 endpoint는 아직 없다.

### 인증된 본문 저장과 작업 결과

`PUT /v1/nodes/{id}/content`는 등록된 기존 문서의 원문을 바꾼다. 신규 저장은 editor 또는 owner만 가능하다. reader로 강등된 기기도 유효한 인증으로 자기 작업의 기존 결과를 조회하거나 동일 요청을 재시도할 수 있다. 만료되거나 폐기된 기기는 두 경로 모두 401로 거절한다. 저장용 기기를 시험하려면 위 관리자 절차에서 editor를 생성해 등록한다.

본문은 최대 25 MiB의 raw bytes다. 편집기 상한 5 MiB와 구별한다. PUT은 본문을 받거나 쓰기 슬롯을 확보하기 전에 token을 검증한다. 인증을 통과한 PUT은 최대 2개까지 처리하고 초과 요청은 본문 수신 전에 429 `busy`로 거절한다. 실제 저장에서도 인증과 쓰기 권한을 다시 검사한다. 현재 HTTP 연결 상한은 16개다. 서버는 BOM, 줄바꿈, 비UTF-8 bytes를 변환하지 않는다. `Content-Encoding`은 생략하거나 `identity` 하나만 지정한다.

응답의 소켓 쓰기가 30초간 진행되지 않으면 연결을 종료해 슬롯을 회수한다. 응답 생성 이후 실제 송신에도 적용하며, 정상적인 쓰기 진행은 정체 기한을 갱신한다. 이 제한은 요청 처리 제한이나 업로드의 120초 수신 제한과 별개다. 응답을 끝까지 받지 못했다면 일부 본문을 완성본으로 사용하지 않는다.

필수 헤더는 다음과 같으며 중복 값은 거절한다.

| 헤더 | 값 |
|---|---|
| `Authorization` | `Bearer <device-token>` |
| `X-Workspace-Epoch` | pairing 또는 server-info에서 확인한 epoch |
| `X-Device-Stream` | 현재 기기에 발급된 `device_stream_id` |
| `X-Operation-Seq` | 새 stream에서 1부터 시작하는 양수 u64의 십진 문자열 |
| `Idempotency-Key` | 클라이언트가 요청 전에 생성하고 보존한 UUIDv7 |
| `If-Match` | 이전 content 응답의 strong ETag 하나 |
| `Content-Digest` | 정확한 전송 bytes의 SHA-256을 base64로 표현한 `sha-256=:BASE64:` |

성공 응답은 200과 `committed` receipt다. operation ID와 순번, epoch, node ID, content/meta revision과 ETag, node ETag, `sha256:` hex hash와 commit 순번을 포함한다. 파일과 부모 디렉터리 영속화 및 metadata/journal/receipt의 DB commit 이후에만 성공을 반환한다. 응답 DTO는 [OperationReceipt와 OperationError](../crates/protocol/src/operation.rs), HTTP 연결은 [저장 adapter](../apps/server/src/http/write.rs)를 따른다.

`GET /v1/operations/{operation_id}`는 Bearer 인증으로 현재 기기 자신의 결과만 반환한다. 회수되지 않은 결과는 성공 여부와 관계없이 200이며 `status`로 생성, 저장, 이동, 삭제와 복원의 성공 및 각 확정 실패, `pending`, `not_applied`, `recovery_conflict`, `abandoned`를 구분한다. 정확한 variant와 필드는 OperationReceipt 계약을 따른다. 다른 기기의 결과와 없는 ID는 모두 404이며 자기 기기의 회수된 결과는 410 `operation_retired`다. ACK 전 과거 결과는 이후 본문 변경이나 과거 객체 삭제에도 유지된다.

PUT의 content 조건 실패는 412, 대상 부재는 404, 삭제된 대상은 410 `node_deleted`, 복구 후 미적용 확정은 409 `operation_not_applied`, 복구 대기는 503 `recovery_pending`이다. 저장된 결과가 있는 이 응답들은 공통 오류 필드에 `operation`을 추가한다. `If-Match` 누락은 428, 잘못된 헤더와 digest는 400, 크기 초과는 413이다. epoch, 순번 간격과 멱등성 충돌은 409다.

응답 유실이나 일시적인 503만으로 작업이 미적용됐다고 판단하지 않는다. 원래 operation ID와 순번으로 조회하거나 원래 헤더, 조건과 bytes를 그대로 재전송한다. 기존 결과가 있으면 다시 저장하지 않고 그 결과를 반환한다. 현재 revision으로 조건을 바꾸는 요청은 새 operation이다. 인증, 형식, digest 오류 등 접수 전 실패는 순번을 소비하지 않는다.

기기별 미회수 receipt 10,000개에서 신규 접수는 429 `receipt_limit`로 중지되며 기존 결과 조회와 동일 요청 재시도는 유지한다. 완료 결과를 로컬에 보존한 뒤 아래 ACK로 회수하면 신규 접수를 재개할 수 있다. 복구 격리 상태에서 신규 쓰기는 차단하지만 자기 작업 결과는 계속 조회할 수 있다.

### 작업 결과 ACK와 abandon

두 endpoint는 Bearer 인증과 `Content-Type: application/json`을 사용하며 JSON 상한은 1 MiB다. 현재 기기의 `workspace_epoch`와 `device_stream_id`를 JSON에 포함한다. reader도 자기 stream의 결과 정리와 미접수 순번 취소를 할 수 있다. 다른 기기의 stream은 403, epoch 불일치는 409, 만료와 폐기는 401로 거절한다.

| 경로 | JSON 필드와 결과 |
|---|---|
| `POST /v1/operation-stream/ack` | `workspace_epoch`, `device_stream_id`, `ack_through`. 200 응답은 같은 binding과 현재 `last_seq`, `retired_through` |
| `POST /v1/operation-stream/abandon` | `workspace_epoch`, `device_stream_id`, `operation_id`, `operation_seq`. 새 취소는 200과 `abandoned` receipt |

모든 순번은 u64 범위의 십진 문자열이다. ACK의 0은 허용하고 abandon은 양수만 받는다. operation ID는 UUIDv7, epoch와 stream은 UUIDv4다. 선언되지 않은 JSON 필드와 잘못된 형식은 400이다.

클라이언트는 terminal 결과와 필요한 충돌 자료를 먼저 로컬 transaction으로 보존한 뒤 연속 구간의 마지막 순번을 ACK한다. 서버는 watermark 갱신과 receipt 회수를 한 transaction으로 처리한다. 미래 순번은 409 `invalid_ack`, pending을 포함하는 구간은 409 `operation_pending`이다. 0 또는 과거 ACK를 다시 보내면 watermark를 낮추지 않고 현재 상태를 반환한다. 복구 대기 상태에서도 이미 완료된 prefix는 ACK할 수 있다.

ACK한 순번 이하의 mutation과 abandon은 410 `operation_retired`로 거절하며 다시 실행하지 않는다. 회수된 operation ID를 새 순번에 붙이면 409 `idempotency_mismatch`다. 서버는 큰 결과 본문과 fingerprint를 회수하지만 이 차단과 ID 기반 GET을 위해 ID, stream, 순번의 작은 기록을 유지한다. 이 식별 기록은 누적되며 일반 본문 객체의 GC는 별도 후속 범위다.

abandon은 아직 서버가 접수하지 않은 바로 다음 순번만 terminal no-op으로 기록한다. 문서 bytes, revision과 head는 바꾸지 않는다. 같은 ID와 순번의 기존 terminal 작업에는 원래 결과를 반환하며 성공 작업을 취소하지 않는다. pending 작업은 409 `operation_pending`으로 거절하고, 신규 취소는 복구 대기 중 503이다. 취소된 ID와 순번으로 늦게 도착한 PUT은 ACK 전에는 409, ACK 후에는 410으로 거절한다. 응답이 유실되면 같은 취소를 재시도하거나 결과를 조회한다.

### 서버 검사와 남은 범위

```bash
cargo test --locked -p notryx-server -p notryx-server-storage -p notryx-protocol
```

120초 업로드 제한 시간은 실제 TCP에서 별도로 검사한다. 이 검사는 기본 회귀 실행에서 제외되며 아래 명령으로 약 2분 동안 실행한다.

```bash
cargo test --locked -p notryx-server --test http_upload progressing_upload_obeys_120_second_total_deadline -- --ignored --exact
```

검사는 임시 합성 저장소에서 localhost HTTP, Unix socket, CLI, 등록, 조회, 조건부 저장, 이동, 삭제, 복원과 재시작을 다룬다. HTTP 저장은 두 기기의 조건 경쟁, 인증과 기기별 결과 격리, 응답을 읽지 못한 저장의 SIGKILL 후 재조회와 재시도를 검사한다. 저장 코어 검사는 admission과 최종 commit의 SQL 실패, 파일 및 디렉터리 영속화 오류 주입, 별도 프로세스의 SIGKILL과 재시작을 포함한다. 자식 프로세스용 ignored 테스트는 부모 테스트가 직접 실행한다. failpoint 도달 대기에는 timeout을 두고 timeout 또는 조기 종료를 실패로 처리하며, 자식 프로세스는 종료 후 wait하여 회수한다. 이 검사 명령의 제공은 실제 전원 장애 시험이나 M1 전체 통과를 뜻하지 않는다.

저장 코어는 content revision, 요청 순번과 digest를 검사한다. 저장 전후의 예상 상태와 다른 파일이 발견되면 자료를 보존하고 쓰기를 막는다. 로컬 관리자가 현재 상태를 검증하고 명시적으로 해결할 수 있다. 다른 프로세스의 동시 파일 변경을 완전히 제어할 수는 없다.

일반 본문 객체의 GC, watcher와 외부 변경 수용, SSE와 journal 동기화, token renew, gateway 인증과 클라이언트 서버 연동은 후속 범위다. 실제 Linux IME와 Android 실기기 검증도 별도로 남아 있다. 실패 주입 hook은 테스트용이며 운영 환경변수로 활성화하지 않는다.
