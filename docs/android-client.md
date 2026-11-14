# Android 개발 시험판

Android는 M5a 제품화 진행 중이다. ARM64 debug APK를 빌드할 수 있지만 실기기에서 현재 버전을 검증하지 않았으므로 지원 완료나 운영판으로 표시하지 않는다.

## 빌드

[개발 환경](development.md)에 기록한 JDK, Android SDK, NDK와 Rust target을 준비한 뒤 저장소 루트에서 실행한다. 에뮬레이터나 연결된 기기는 필요하지 않다.

```bash
source scripts/dev-env.sh
npm ci
CARGO_BUILD_JOBS=1 npm run tauri -- android build --debug --target aarch64 --apk --ci
```

결과는 `apps/client/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`다. 파일 이름에 `universal`이 있지만 위 명령은 ARM64만 빌드한다. 개발용 debug 서명을 사용하며 공개 배포용 서명을 대신하지 않는다. 앱 식별자는 기존 데이터 경로와의 호환성을 위해 `org.notryx.m0`를 유지한다.

빌드한 APK는 다음 명령으로 검사한다.

```bash
python3 scripts/test-android-apk.py
python3 scripts/check-android-apk.py \
  apps/client/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk \
  --build-type debug --output .artifacts/android-debug-check.json
```

검사는 앱 ID, 버전 이름, 표시 이름과 시작 Activity, SDK 범위, backup 비활성화, debug와 평문 통신 설정, ARM64 라이브러리, 16 KiB 정렬과 JNI 인증정보 클래스의 포함 여부를 확인한다. APK의 `assets/`에는 Notryx MIT 원문, 프런트엔드와 Mermaid 번들의 고지 및 소스 안내, Android 대상 Rust 고지를 포함하며, 검사에서 누락되거나 빈 파일을 거절한다. debug APK는 서명 유효성도 확인한다. 결과 JSON에 파일 크기와 SHA-256을 남긴다. 설치나 실제 JNI 호출을 검사하지는 않는다.

검사 결과의 `--output`에는 입력 APK와 다른 파일을 지정한다. 같은 경로, 심볼릭 링크 또는 하드 링크로 입력 APK를 가리키면 검사 전에 거절한다.

## CI와 release 빌드

`check` workflow의 `android` 작업은 ARM64 debug APK 빌드, 암호문 파일의 JVM 시험과 APK 검사를 수행한다. 개발용 APK와 검사 JSON을 `android-arm64-debug-development` artifact로 7일간 보관한다. 실행마다 개발용 서명 키가 달라질 수 있으므로 이 파일을 기존 앱의 업데이트용 배포물로 취급하지 않는다. CI는 APK를 GitHub Release나 앱 스토어에 게시하지 않는다.

release 빌드는 debug와 구분해서 검사한다.

```bash
source scripts/dev-env.sh
CARGO_BUILD_JOBS=1 npm run tauri -- android build --target aarch64 --apk --ci
python3 scripts/check-android-apk.py \
  apps/client/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk \
  --build-type release --allow-unsigned --output .artifacts/android-release-check.json
```

기본 release 빌드에는 배포용 서명이 없다. 위 명령의 결과는 설치 배포용 APK가 아닌 빌드 검증 자료다. `--allow-unsigned`는 서명을 검사하지 않는 개발용 옵션이다. 이 옵션이나 예상 서명자 지문을 지정하지 않으면 release 검사는 실패한다.

Android 빌드와 개발 실행의 준비 단계에서 잠긴 Cargo 의존성을 먼저 가져온 뒤 `prepare-rust-notices.py --target aarch64-linux-android`를 실행한다. 고지 수집 자체는 오프라인 metadata를 사용한다. crate에 원문이 없는 항목은 버전, upstream commit과 해시를 고정한 [보충 고지](../third-party/rust/README.md)를 사용한다. Rust 일반 및 빌드 의존성을 수집하므로 실제 링크된 코드만의 목록은 아니다. Gradle 의존성이 제공하는 `META-INF` 고지도 유지한다. 고지 포함 검사는 전체 라이선스 적합성 판정을 대신하지 않는다.

## 배포용 APK 서명과 검증

서명은 위 release 빌드가 끝난 뒤 별도 명령으로 수행한다. Tauri와 Gradle에 비밀번호를 전달하지 않는다. Android SDK의 [apksigner](https://developer.android.com/tools/apksigner)를 사용하며, 기존 빌드에서 정렬한 APK를 서명 전에 검사하고 서명 후에도 다시 검사한다.

고정 배포 키와 암호는 저장소 밖에서 관리하고 별도로 백업한다. 아래 경로와 alias는 준비한 키에 맞게 바꾼다. 신뢰한 키의 인증서 SHA-256 지문은 `keytool -list -v -keystore /path/to/notryx-release.jks -alias notryx-release`로 확인할 수 있다. 내려받은 APK에서 읽은 지문을 곧바로 신뢰값으로 사용하지 않는다.

```bash
source scripts/dev-env.sh
python3 scripts/sign-android-apk.py \
  apps/client/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk \
  --keystore /path/to/notryx-release.jks --alias notryx-release \
  --expected-signer-sha256 '<trusted-certificate-sha256>' \
  --output .artifacts/releases/Notryx_0.1.0_arm64.apk
```

키 저장소 암호와 키 암호는 터미널에서 숨김 입력한다. 자동 실행에서는 비밀 관리 기능이 `NOTRYX_ANDROID_STORE_PASSWORD`와 `NOTRYX_ANDROID_KEY_PASSWORD`를 제공해야 한다. 암호 값 자체를 명령행, 소스, GitHub 공개 변수나 로그에 넣지 않는다. 스크립트는 암호를 자식 프로세스의 환경에서 제거하고 apksigner의 표준 입력으로만 전달한다. 이번 CI에는 배포 키나 배포 서명 작업을 추가하지 않았다.

서명 도구는 입력 APK와 기존 출력 파일을 덮어쓰지 않는다. 임시 파일에 서명하고 APK 검사, 유효한 v2 서명, 단일 서명자 및 지정한 인증서 SHA-256 일치를 확인한 뒤 새 출력 파일을 만든다. 실패하면 검증하지 못한 APK를 출력 경로에 남기지 않는다. 여러 서명자나 키 회전은 현재 서명 경로에서 지원하지 않는다.

오류에는 실패한 단계를 표시한다. `Input APK inspection`은 입력 release APK를 검사 명령으로 확인하고, `APK signing`은 keystore, alias와 암호를 점검한다. `Signed APK verification`은 예상 서명자 지문과 서명 후 패키지 검증을 확인한다. 원시 도구 출력과 암호는 오류 안내에 포함하지 않는다.

배포 파일을 받은 쪽에서도 별도로 신뢰한 지문으로 검사한다.

```bash
python3 scripts/check-android-apk.py /path/to/Notryx_0.1.0_arm64.apk \
  --build-type release --expected-signer-sha256 '<trusted-certificate-sha256>' \
  --output .artifacts/android-signed-check.json
```

성공 결과의 `expected_signer_verified`는 호출자가 제공한 지문과 일치한다는 뜻이다. APK 자체가 공식 발행자를 증명하는 신뢰 기준을 제공하지 않는다. 검증 후 설치하는 절차이며, 사용자가 검사 도구를 건너뛰고 직접 설치하는 동작을 이 스크립트가 차단하지는 않는다.

서명 검증의 자동 시험은 실제 미서명 release APK와 임시 시험 키를 사용한다. 시험 키와 서명 APK는 종료 시 삭제한다.

```bash
python3 scripts/test-android-signing.py \
  --apk apps/client/src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk \
  --output .artifacts/android-signing-test.json
```

정상 서명, unsigned, 다른 서명자, 서명 뒤 변조, 암호 오류와 입력 및 기존 출력 보존을 검사한다. 현재 공식 배포 키는 준비하지 않았으며, 동일 키 업데이트 시 Android의 설치 동작과 기존 자료 보존은 실기기 미검증이다. 이 자동 시험을 AT-47의 전체 통과로 표시하지 않는다.

## 현재 화면 범위

폭 650 CSS px 이하에서는 문서 목록, 읽기, 편집을 각각 한 화면에 표시한다. 문서를 선택하면 읽기로 시작하고 편집 버튼으로 원문을 연다. 1 MiB를 넘는 문서는 Markdown 미리보기 대신 읽기 전용 원문 화면을 사용한다. 기존 편집 가능 크기와 원문 보존 규칙은 그대로 적용한다.

편집에서 읽기로 돌아갈 때는 먼저 기기 초안을 저장한다. 저장이 실패하면 편집 화면과 입력을 유지한다. 한글 조합 중에는 화면 전환을 보류한다. 목록과 읽기 화면을 오갈 때 열린 편집기와 undo 기록을 유지하며, 다른 문서를 열 때는 기존 문서 전환 절차를 따른다.

모바일 화면의 자동 검사는 다음과 같다.

```bash
npm test -- apps/client/src/mobile-navigation-main.test.ts
CARGO_BUILD_JOBS=1 npm run tauri -- build --debug --no-bundle -- --locked -j 1
xvfb-run -a python3 scripts/m5a-mobile-webdriver.py
```

첫 검사는 UI 진행과 저장 실패를 검사한다. 두 번째 실행 환경은 화면을 줄인 Linux WebKitGTK와 합성 기기 저장소다. Android WebView나 실기기 키보드 검증 결과로 해석하지 않는다.

## 편집 보조 도구와 화면 여백

좁은 편집 화면 아래에 제목, 목록, 코드, 링크 삽입 버튼을 제공한다. 제목과 목록은 선택한 줄 앞에 각각 `## `와 `- `를 추가한다. 코드는 선택 내용을 fenced 코드 블록으로 감싸며, 링크는 선택 텍스트를 링크 이름으로 보존하고 URL 입력 부분을 선택한다. 링크 이름의 대괄호와 역슬래시는 Markdown escape로 표시 문자를 보존한다. 코드와 링크는 선택이 없으면 입력할 예시 텍스트를 넣고 선택한다.

삽입은 기존 CodeMirror 변경 및 저장 경로를 사용한다. 한 번의 undo로 삽입을 되돌릴 수 있고, 읽기 전용과 BOM을 포함한 5 MiB 제한을 유지한다. 조합 중에는 삽입을 보류한다. 도구를 누를 때 편집기 선택과 DOM 포커스를 유지하며, 나머지 문서 동작은 버튼 줄을 가로로 스크롤해 사용할 수 있다.

Android는 native 컨테이너에서 시스템 바, 화면 cutout과 IME의 겹치는 영역을 제외해 WebView 크기를 조절한다. 처리한 inset은 0으로 전달해 CSS 여백과 키보드 영역을 중복 적용하지 않는다. `adjustResize`도 설정한다. 이 연결은 [Android의 WebView inset 안내](https://developer.android.com/develop/ui/views/layout/webapps/understand-window-insets)를 따른다.

Linux WebView의 320×360 화면에서 버튼과 저장 상태가 보이고 편집 공간이 남는 것을 확인했다. 높이를 줄인 창의 검사이며 실제 Android 키보드, 회전, cutout과 시스템 제스처 영역의 검증은 아니다.

## 미리보기와 첨부 표시

기본 Markdown 미리보기는 raw HTML을 실행하지 않고 외부 이미지를 자동으로 요청하지 않는다. 문서 안의 로컬 이미지 참조만 등록된 서버에서 가져온다. Rust가 응답의 revision, digest, 크기를 확인하고 PNG, JPEG, WebP를 디코딩한 뒤 PNG로 변환해 표시한다. SVG와 애니메이션은 표시하지 않으며 첨부 원본은 변경하지 않는다.

Android의 Mermaid 블록은 기본적으로 원문과 안내를 표시한다. 개발 APK에는 시험용 렌더링 경로를 추가했다. Tauri와 별개의 Android WebView에서 전용 번들을 실행하며 JavaScript에서 호출할 native bridge는 제공하지 않는다. native가 결과를 읽고 기존 SVG 검사와 수동 이미지 표시 경로로 전달한다. main WebView에 Mermaid 실행 코드나 비신뢰 HTML iframe을 추가하지 않는다.

시험용 WebView는 고정 번들 외 요청, 파일 및 content 접근, navigation과 새 창을 차단하도록 구성했다. 입력 64 KiB, 결과 SVG 2 MiB, 동시 2개와 15초 수명 제한을 적용한다. timeout과 WebView 폐기는 별도 프로세스나 CPU 강제 종료 보장이 아니다. 실제 Android의 렌더링과 격리를 검증하지 않았으므로 일반 debug 실행과 release에서는 활성화하지 않는다.

기기 시험 시 기기 저장을 마친 합성 시험 데이터에서만 다음 명령을 사용한다. `-S`는 실행 중인 시험 앱을 종료한 뒤 다시 연다. debug APK에만 적용되며 release는 flag를 무시한다.

```bash
adb shell am start -S -n org.notryx.m0/.MainActivity --ez notryxMermaidPreview true
```

기본 원문 표시로 돌아가려면 같은 명령의 마지막 값을 `false`로 바꾼다. 시험에서는 정상 그래프와 오류 원문 표시뿐 아니라 native bridge 부재, 외부 요청과 파일 및 APK asset 접근 차단, iframe과 새 창 차단, 취소와 앱 복귀를 확인해야 한다. 자동 정책 검사와 APK 빌드는 이 실기기 검증을 대신하지 않는다.

외부 링크는 사용자가 누를 때 Rust에서 현재 renderer, 편집 세션과 `http/https` URL을 검사한 뒤 Android의 `ACTION_VIEW`와 `BROWSABLE`로 외부 처리 앱을 연다. 앱 내부 WebView로 이동하거나 임의 intent 및 파일 경로를 실행하지 않는다. 처리 앱이 없거나 실행에 실패하면 오류를 표시하고 편집 세션을 유지한다. 앱 목록 조회 권한이나 별도 플러그인은 추가하지 않는다. [Android 외부 URL 실행 안내](https://developer.android.com/training/package-visibility/use-cases#open-urls)

공통 경로의 자동 검사와 Linux WebView 검사는 Android WebView의 CSP, 실제 첨부 표시와 외부 앱 실행 검증을 대신하지 않는다.

## 뒤로 가기와 앱 복귀

Android 시스템 뒤로 가기는 키보드가 보이면 먼저 키보드를 숨기도록 연결했다. 이후 좁은 화면에서는 편집, 읽기, 목록 순서로 돌아간다. 편집에서 나가기 전에 기기 초안 저장을 기다리며, 저장 실패 또는 한글 조합 중에는 화면을 유지한다. 저장 대기 중 새 조합이 시작된 경우도 전환을 보류한다.

목록이나 넓은 화면에서 뒤로 가기를 누르면 기기 저장 후 앱을 백그라운드로 보낸다. 편집 세션을 닫거나 앱 프로세스를 종료하지 않아 같은 프로세스로 복귀할 때 입력과 undo를 유지한다. 초기화나 복구 화면처럼 편집 UI가 아직 준비되지 않은 상태에서는 시스템 뒤로 가기를 처리하지 않는다. 이때는 Android 홈 동작으로 나갈 수 있다.

Android의 pause 알림에서 수정된 초안을 저장하고, resume 알림에서는 실패한 저장을 다시 시도한 뒤 열린 본문과 동기화 상태를 확인한다. 원격 갱신 실패로 잠긴 문서의 복구 안내는 유지한다. native 동기화는 resume 때 변경 이력 재조회를 시작한다. OS가 알림 없이 프로세스를 종료할 수 있으므로 기존 주기적 기기 저장은 유지한다.

자동 시험은 화면 전환, 저장 실패, 조합 경합, 편집기와 undo 보존을 검사한다. Linux WebView에서는 뒤로 가기 이벤트 후 실제 SQLite checkpoint와 새 background 명령의 renderer 격리를 확인한다. 실제 Android 키보드 숨김, 제스처, pause/resume 알림과 앱 복귀는 실기기 미검증이다.

## 인증정보 보호 저장

Android 인증정보는 Rust의 기존 credential adapter에서 JNI로 Android Keystore 기반 저장소를 호출한다. Keystore의 AES-256 키로 등록 정보를 GCM 암호화하고, 암호문은 앱의 `noBackupFilesDir`에 보관한다. 등록 정보에는 서버 origin, workspace, epoch, device와 stream이 포함된다. renderer, SQLite와 평문 파일에는 token을 보관하지 않는다. [Android Keystore](https://developer.android.com/privacy-and-security/keystore), [백업 제외 디렉터리](https://developer.android.com/reference/android/content/Context#getNoBackupFilesDir())

등록은 기존 pending 기록, 보호 저장, 읽기 확인, 등록 활성화 순서를 따른다. 저장을 시작하기 전에 Keystore를 사용할 수 없으면 세션 전용 인증을 사용한다. 쓰기 결과를 확인할 수 없으면 pending 등록과 기존 초안을 유지한다. 암호문 손상이나 키 접근 실패 시 기존 파일을 지우거나 읽기 과정에서 키를 새로 만들지 않는다.

앱 시작 시 기존 복구 경로가 저장된 등록 정보를 읽고 서버의 workspace, epoch, device와 stream을 확인한 뒤 세션을 복원한다. 확인에 실패하면 자동 전송을 보류하고 초안, base와 outbox를 보존한다. 구현과 실기기 검증은 구분한다. 현재 JNI 호출과 실제 Android Keystore의 앱 재시작 복구는 실기기에서 확인하지 않았다.

보호 저장 자동 검사는 다음과 같다.

```bash
source scripts/dev-env.sh
CARGO_BUILD_JOBS=1 cargo test --locked -j 1 -p notryx-client credential_ --lib
apps/client/src-tauri/gen/android/gradlew \
  -p apps/client/src-tauri/gen/android :app:testArm64DebugUnitTest \
  --tests org.notryx.m0.CredentialFilesTest \
  --max-workers=1 --no-daemon --console=plain
```

Rust 검사는 등록 복구와 실패 시 SQLite 자료 보존을 검사한다. JVM 검사는 실제 AES-GCM과 임시 파일로 암호문 재열기, 변조 거절, 중복 쓰기 및 잘못된 삭제 거절, 키 접근 실패와 파일 동기화 실패를 검사한다. JVM의 시험용 키 공급자를 사용하므로 실제 Android Keystore 검증을 대신하지 않는다.

## 남은 Android 작업

- 실기기에서 Android Keystore 저장, 앱 재실행 후 인증 복구와 키 접근 실패 시 자료 보존 검증.
- 편집 보조 도구, 키보드 및 시스템 화면 여백, Android 뒤로 가기와 앱 복귀의 실기기 검증.
- Android 실기기의 미리보기 격리, 첨부 이미지 표시와 외부 링크 실행 검증. 현재 Mermaid는 원문 fallback을 사용한다.
- Gboard와 삼성 키보드의 한글 조합, 선택, 회전, 앱 복귀 시험과 공개 배포용 서명 검증.

기기 초안 보존과 인증정보 보관은 다른 계약이다. APK 빌드 성공이나 Linux 화면 검사를 Android의 인증 복구, 물리 입력, 전원 장애 검증으로 간주하지 않는다.
