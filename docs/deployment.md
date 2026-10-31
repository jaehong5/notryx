# Linux Docker 개발 서버

Docker에는 Rust 서버만 실행한다. Tauri 앱은 컨테이너에 포함하지 않는다. 현재 구성은 M3 서버의 격리된 개발과 검증용이다. 두 Linux 앱의 통합 시험은 별도로 수행하며, 이 Docker 구성의 검증을 운영판 출시나 운영 서비스 전환 완료로 해석하지 않는다.

Cloudflare 등 외부 업체의 계정, 터널이나 인증 서비스는 필요하지 않다. 외부 접속은 운영자가 선택한 HTTPS reverse proxy 또는 적절히 보호된 사설 경로로 구성하며, Notryx 자체 기기 인증을 유지한다. 공급자 전용 gateway 인증 연동은 M4 기본 배포 조건에 포함하지 않는다.

## 이미지 빌드

저장소 루트에서 실행한다.

```bash
docker build -f deploy/Dockerfile --target runtime -t notryx:dev .
```

Dockerfile은 Rust builder와 Debian runtime의 digest를 고정한다. `Cargo.lock`을 유지하고 Rust `1.99.1`로 서버만 빌드한다. 빌드 작업은 두 개로 제한한다. workspace의 클라이언트 manifest와 Rust target 파일은 잠긴 의존성 해석에 필요하지만, Tauri와 Android target은 빌드하거나 설치하지 않는다.

`.dockerignore`는 빌드에 필요한 manifest와 Rust 코드만 허용한다. Git 이력, AGENTS, 설계와 결과 문서, 환경 파일, 로컬 데이터와 artifacts는 빌드 context에서 제외한다.

| 구성 요소 | 이미지 구성 |
|---|---|
| Rust | `1.99.1`, Cargo lockfile 사용 |
| 서버 ABI | Linux glibc, Debian bookworm runtime |
| SQLite | SQLx의 bundled SQLite를 서버 빌드에 포함 |
| HTTP | 서버에서 HTTP 제공. 외부 HTTPS는 별도 reverse proxy에서 처리 |
| 외부 연결 | 서버 기본 기능에 원격 콘텐츠 요청이나 외부 AI 연결 없음 |
| 실행 계정 | 기본 `1000:1000`, Compose에서 호스트 UID/GID 지정 |

`notryx:dev`는 로컬 빌드 태그다. 공개 레지스트리 이미지나 서명된 릴리스가 아니다. 실제 배포에서 사용할 이미지 digest와 플랫폼별 실행 결과는 릴리스별로 기록해야 한다. amd64 실행 결과만으로 arm64 지원을 선언하지 않는다.

## 새 시험 저장소 실행

Linux 사용자에게 Docker 접근 권한이 있어야 한다. 아래 `.local/docker`는 새 시험 디렉터리로 사용한다. 기존 문서 저장소에 초기화 명령을 다시 실행하지 않는다.

```bash
mkdir -p .local/docker/notes .local/docker/state
chmod 700 .local/docker/notes .local/docker/state
cp deploy/server.toml.example .local/docker/server.toml
chmod 600 .local/docker/server.toml

export NOTRYX_IMAGE=notryx:dev
export NOTRYX_UID="$(id -u)"
export NOTRYX_GID="$(id -g)"
export NOTRYX_HTTP_PORT=8080
export NOTRYX_NOTES_DIR="$PWD/.local/docker/notes"
export NOTRYX_STATE_DIR="$PWD/.local/docker/state"
export NOTRYX_CONFIG_FILE="$PWD/.local/docker/server.toml"

docker compose --env-file /dev/null -f deploy/compose.yml config --quiet
docker compose --env-file /dev/null -f deploy/compose.yml run --rm --no-deps server init --config /etc/notryx/server.toml
docker compose --env-file /dev/null -f deploy/compose.yml up -d --pull never
```

`deploy/.env.example`에도 같은 변수를 제공한다. 별도 환경 파일을 만들었다면 `--env-file`에 그 경로를 지정한다. notes, state와 config 경로는 절대 경로여야 하며 실행 전에 존재해야 한다. Compose는 누락된 bind 경로를 생성하지 않는다.

서버 프로세스는 지정한 UID/GID로 처음부터 실행한다. 시작하면서 root 권한으로 문서의 소유권을 바꾸는 entrypoint는 없다. state는 실행 UID가 소유한 `0700` 디렉터리여야 한다. notes와 state는 별도 경로를 사용한다.

포트는 호스트 `127.0.0.1`에만 공개한다. 다른 컨테이너의 reverse proxy를 연결하려면 별도 private network 구성을 마련해야 한다. 앱의 기기 인증은 reverse proxy나 사설 네트워크 사용 여부와 관계없이 유지한다.

## 상태와 관리자 명령

```bash
docker compose --env-file /dev/null -f deploy/compose.yml ps
docker compose --env-file /dev/null -f deploy/compose.yml exec server /app/notryx-server healthcheck --address 127.0.0.1:8080
docker compose --env-file /dev/null -f deploy/compose.yml exec server /app/notryx-server admin --config /etc/notryx/server.toml status
```

healthcheck는 `/health/ready`의 HTTP 2xx를 확인한다. 전체 검사 시간은 2초, 상태 줄은 1 KiB로 제한하며 redirect를 따라가지 않는다. 별도 curl 패키지나 DB 접근 권한은 필요하지 않다. 복구가 남아 readiness가 false면 unhealthy가 된다. Compose는 unhealthy 표시만으로 컨테이너를 자동 재시작하지 않는다.

관리자 socket은 실행 UID가 소유한 `/run/mdw` tmpfs에 만들며, 재시작 때 다시 생성한다. 관리자 명령과 기기 등록 절차는 [개발 안내](development.md#로컬-관리자와-기기-등록)를 따른다. pairing 발급 결과에는 일회용 credential이 있으므로 공개 로그나 이슈에 붙이지 않는다.

컨테이너 root filesystem은 읽기 전용이다. 문서와 상태는 지정한 bind mount에 저장한다. `/tmp`와 `/run/mdw`만 임시 영역이며, 재시작 후 필요한 원문, intent와 복구 자료를 두지 않는다. 백업 기능은 이 Compose 구성에 포함하지 않는다.

## 종료와 schema 이전

```bash
docker compose --env-file /dev/null -f deploy/compose.yml stop
docker compose --env-file /dev/null -f deploy/compose.yml run --rm --no-deps server migrate --config /etc/notryx/server.toml
docker compose --env-file /dev/null -f deploy/compose.yml up -d --pull never
```

서버를 중지한 뒤 notes와 state 전체의 독립 사본을 확보하고 schema 이전을 수행한다. state에는 DB와 남은 WAL, 객체 및 복구 자료가 포함된다. DB 파일 하나만 복사하지 않는다. 이전 실패를 초기화 명령이나 state 삭제로 해결하지 않는다.

## 격리된 검증

```bash
docker build -f deploy/Dockerfile --target verify -t notryx:verify .
python3 scripts/test-docker.py --image notryx:dev --output .artifacts/m1-docker-results.json
```

`verify` stage는 서버와 파일 저장소의 Rust 시험을 컨테이너 안에서 실행한다. Python 스크립트는 이미 빌드한 이미지로 합성 자료와 별도 시험 컨테이너만 만든다. 실제 실행에는 로컬 Docker socket과 root가 아닌 Linux 사용자가 필요하다.

현재 스크립트의 검사 범위는 다음과 같다.

- 호스트와 컨테이너의 UID/GID, 공유 파일 쓰기, 다른 UID의 관리자 접근 거절.
- 원문 저장, 실제 SIGKILL과 재시작, 기존 receipt 재현.
- 업로드 staging의 재시작 보존, 5 MiB 초과 첨부 연결, 소비된 업로드의 같은 생성 결과 재현.
- 빈 mount, 누락된 bind 경로, workspace marker 불일치 거절.
- 실제 디렉터리 권한 거절과 읽기 전용 notes mount에서 원문 보존 및 복구.
- 별도 16 MiB state tmpfs의 실제 ENOSPC, 업로드와 저장의 507 응답, 공간 해제 후 재시도.
- 시작 전에 있던 컨테이너의 실행과 재시작 상태 보존.

시험용 pairing과 token은 메모리에만 두며 결과 JSON에 기록하지 않는다. 시험 컨테이너는 고유한 이름으로 추적하고 생성한 컨테이너와 임시 디렉터리만 정리한다. `docker prune`이나 다른 서비스의 중지 명령은 사용하지 않는다.

이 스크립트는 장치 전원 장애, Linux IME, Android 실기기, 두 클라이언트 동기화나 운영 서비스 전환을 검증하지 않는다. 첨부 업로드의 제품 동작과 격리 해결 중 장애 시험은 관련 서버 시험 결과와 함께 확인해야 한다.
