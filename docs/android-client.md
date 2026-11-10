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

Android의 Mermaid 블록은 현재 원문과 안내를 표시한다. main WebView에 Mermaid 실행 코드나 비신뢰 HTML iframe을 추가하지 않는다. 현재 Android IPC는 호출 프레임을 별도로 구별하지 않으므로 격리된 렌더링 환경의 실기기 검증 전까지 이 제한을 유지한다.

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
