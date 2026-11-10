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

## 남은 Android 작업

- 실제 인증정보의 Android Keystore 저장과 재실행 후 복구. 현재 Android 인증은 세션 전용이다.
- 편집 보조 도구, Android 뒤로 가기와 앱 복귀 동작, 키보드 및 시스템 화면 여백의 실제 확인.
- Android의 미리보기 격리. 현재 Mermaid는 원문 fallback을 사용한다.
- Gboard와 삼성 키보드의 한글 조합, 선택, 회전, 앱 복귀 시험과 공개 배포용 서명 검증.

기기 초안 보존과 인증정보 보관은 다른 계약이다. APK 빌드 성공이나 Linux 화면 검사를 Android의 인증 복구, 물리 입력, 전원 장애 검증으로 간주하지 않는다.
