# Notryx 개발 환경

이 문서는 Ubuntu 24.04 x86_64에서 Linux와 Android ARM64 개발에 사용하는 도구와 확인 명령을 정리한다. 현재 `0.1.0`은 M0 시험 앱이며 domain, protocol, client-store와 Tauri 편집기로 구성된다. 서버 저장 코어의 조건부 본문 저장과 재시작 복구도 검사할 수 있다. 실행 가능한 HTTP 서버와 동기화는 아직 없다.

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


## 서버 저장 코어 검사

Linux에서 실제 임시 파일과 SQLite를 사용한다. 별도 서버나 운영 데이터는 필요하지 않다.

```bash
cargo test --locked -p notryx-server -p notryx-server-storage
```

`apps/server`는 내부 저장 라이브러리이며 HTTP listener, 인증 또는 배포 명령을 제공하지 않는다. 초기화는 비어 있는 두 디렉터리에서만 허용한다. 명시적으로 등록한 루트 바로 아래의 일반 파일을 대상으로 content revision, 요청 순번과 digest를 검사한다. 저장 전후 상태와 다른 파일이 발견되면 자료를 보존하고 쓰기를 막으며, 관리자 해결 기능은 아직 없다. 외부 변경 자동 수용, 폴더와 CRUD API, receipt ACK 및 GC도 후속 범위다.

파일시스템 어댑터는 Linux `openat2`를 요구하며 지원하지 않는 환경에서는 시작을 거절한다. state 디렉터리는 `0700`으로 관리한다. 다른 프로세스의 동시 파일 변경을 완전히 제어할 수는 없다. 저장 코어의 파일 I/O는 동기 방식이므로 HTTP 서버를 붙일 때 전용 실행 경계에 배치해야 한다.

테스트는 admission과 최종 commit의 SQL 실패, 파일 및 디렉터리 영속화 오류 주입, 별도 프로세스의 SIGKILL과 재시작을 포함한다. 자식 프로세스용 ignored 테스트는 부모 테스트가 직접 실행한다. failpoint 도달 대기에는 timeout을 두고 timeout 또는 조기 종료를 실패로 처리하며, 자식 프로세스는 종료 후 wait하여 회수한다. 이 결과는 실제 전원 장애 시험이나 전체 서버의 수용 시험을 대신하지 않는다. 실패 주입 hook은 테스트용이며 운영 환경변수로 활성화하지 않는다.
