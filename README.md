# Notryx

파일 기반 셀프호스팅 Markdown 작업 공간을 개발하고 있다. 현재 버전 `0.1.0`의 클라이언트는 합성 문서의 로컬 편집과 초안 저장을 제공한다. Linux 개발 서버는 초기화, 로컬 관리자 명령, 기기 등록과 인증된 문서 조회를 제공한다. 저장 코어에는 조건부 본문 저장과 재시작 복구가 있지만 원격 쓰기 API와 클라이언트 서버 연결, 동기화는 아직 없다. M0와 M1 전체 완료를 뜻하지 않는다.

- 설치와 클라이언트 실행: [개발 안내](docs/development.md)
- 개발 서버 실행과 기기 등록: [서버 안내](docs/development.md#개발-서버-실행과-조회)
- 실기기 시험: [M0 수동 검증](docs/m0-manual.md)

## 실행

개발 도구를 설치한 Bash에서 실행한다.

```bash
source "${NVM_DIR:-$HOME/.nvm}/nvm.sh"
nvm use
source scripts/dev-env.sh
npm ci
npm run tauri -- dev
```

새 문서는 LF를 사용한다. 기존 문서를 수정하지 않으면 bytes를 유지하며, 유효 UTF-8 문서를 실제 편집하면 전체 줄바꿈을 LF로 저장한다. CRLF, 혼합 줄바꿈과 단독 CR 문서는 편집 전에 저장 정책을 안내한다. BOM과 알 수 없는 문법을 유지하고 비UTF-8 및 크기 초과 문서는 읽기 전용으로 연다.

시험 앱은 OS 앱 데이터 디렉터리의 `org.notryx.m0`에 `drafts.sqlite3`를 만든다. 기존 서버나 SilverBullet에는 연결하지 않는다. 별도 시험 저장소를 지정하려면 native 프로세스 환경변수 `NOTRYX_M0_DATA_DIR`를 사용한다. 초안은 평문 SQLite에 저장된다.

## 검사

```bash
bash scripts/check.sh
cargo test --locked -p notryx-server -p notryx-server-storage
npm run tauri -- build --debug --no-bundle
npm run tauri -- android build --debug --target aarch64 --apk --ci
```

Android 실기기와 두 한글 키보드 시험을 통과하기 전에는 Android 지원이나 M0 전체 통과를 선언하지 않는다. Linux 자동 입력 시험도 실제 IME 검증을 대신하지 않는다.

프로젝트 라이선스와 공개 배포 안내는 아직 확정하지 않았다. 현재 코드를 라이선스가 부여된 공개 릴리스로 취급하지 않는다.
