# M0 실기기 검증

아래 항목은 LF 편집 및 재로드 복구 정책을 기준으로 자동 테스트 결과와 별도로 실제 기기에서 기록해야 한다. 실기기 결과 없이 M0 전체를 통과로 처리하지 않는다.

## 설치와 기록

```bash
source scripts/dev-env.sh
adb devices
adb install -r apps/client/src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
adb logcat -s NotryxM0
```

APK 경로는 빌드 출력의 실제 위치와 대조한다. 로그에는 `credential_probe_passed=true` 또는 `false`만 표시한다. probe는 합성 문자열로 Android Keystore AES-GCM 암호화와 복호화를 실행하고 생성한 키를 삭제한다. 제품 credential의 영구 저장, 재시작, 분실과 잠금 정책 검증은 아니다.

기기 모델, Android 버전, System WebView 버전, 키보드 앱과 버전, 앱 빌드 및 실행 일자를 기록한다. Gboard와 삼성 키보드는 각각 별도 결과를 남긴다.

## 한글과 편집

| 행동 | 확인할 결과 |
|---|---|
| `한글 입력`, emoji와 결합문자 입력 | 글자 또는 조합 중 입력이 유실되지 않음 |
| 조합 중 커서 이동과 자동 완성 | 예상한 문자열과 위치 유지 |
| 10초 이상 지속 입력 | 입력 중 로컬 checkpoint가 갱신됨. 마지막 저장 완료 세대 이후 입력과 구별 |
| 조합 중 저장 버튼 | 조합을 강제로 확정하지 않고 현재 편집 내용 보존 |
| 조합 중 문서 전환 또는 창 닫기 | 조합 완료 요청을 표시하고 기존 문서를 유지 |
| 선택 핸들과 하드웨어 키보드 | 선택, 삭제, undo/redo, 저장 단축키 정상 |
| 회전과 화면 잠금 및 복귀 | 편집 내용 보존, keyboard inset과 safe area 확인 |
| 100 KiB와 5 MiB 문서 편집 및 붙여넣기 | 오류와 실제 지연 기록. 성능 목표를 낮춰 합격 처리하지 않음 |
| 저장 완료 후 앱 강제 종료와 다시 열기 | 완료한 문서와 generation 복구 |
| BOM을 포함한 LF, CRLF, 혼합 줄바꿈과 단독 CR 문서를 열고 미수정 저장 | 원래 bytes와 끝 줄바꿈 유무를 그대로 유지 |
| 유효 UTF-8의 CRLF, 혼합 줄바꿈과 단독 CR 문서를 한글 IME로 편집 | 수정 전에 `수정하면 LF로 저장` 안내. 저장 후 전체 줄바꿈은 LF이며 BOM과 알 수 없는 문법 유지 |
| 편집 후 undo로 원래 text 복원 | 편집한 generation이므로 LF로 저장. 최초 base의 원래 bytes는 유지 |
| 저장 완료 뒤 WebView 재로드 후 문서 재열기와 추가 편집 | 완료 checkpoint와 generation을 복구하고 새 session에서 편집/저장 가능. base 유지 |
| 미수정 문서의 WebView 재로드 | 원래 bytes 유지, 재로드만으로 LF 변환 없음 |
| 비UTF-8와 5 MiB 초과 문서 | 읽기 전용으로 표시하고 원본을 수정하지 않음 |

재로드 시험은 저장 완료를 확인한 뒤 실행한다. 재로드 직전 commit하지 못한 입력은 복구 보장 대상이 아니다. 재로드 뒤 정상 화면과 저장을 확인하고 완료 generation, checkpoint bytes와 base를 자동 검증 결과와 대조한다.

Linux 데스크톱에서도 실제 사용 IME와 키보드 정보를 남기고 같은 한글 조합 항목을 확인한다. WebDriver의 문자열 주입은 실제 IME 조합, 선택 핸들 또는 키보드 앱 동작을 증명하지 않는다.

## 격리

`격리 시험 열기`에서 Android의 sandboxed iframe 결과를 확인한다. 부모 문서 접근 차단, native bridge 접근 차단 또는 부재, CSP에 의한 외부 연결 차단이 각각 확인되어야 한다. DNS 오류만으로 외부 연결 차단을 통과 처리하지 않는다. Linux는 별도 WebView와 iframe을 각각 검사한다.

결과가 불확실하거나 실패하면 Android의 능동 renderer는 활성화하지 않는다. 현재 앱은 Markdown preview와 Mermaid를 제공하지 않으며 원문 편집만 제공한다.
