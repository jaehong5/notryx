# 개발 의존성 기록

2026-10-27의 `Cargo.lock`, `package-lock.json` 기준이다. Rust와 npm의 직접 의존성은 각 manifest에서 버전을 고정하고 전이 의존성은 lockfile로 고정한다. 현재 앱 버전은 `0.1.0`, 프로토콜 식별자는 `mdw-sync/1.2`다.

| 영역 | 주요 직접 의존성 |
|---|---|
| 순수 계약 | uuid 1.29.0, serde 1.0.229, serde_json 1.0.154, schemars 1.2.3, ts-rs 12.0.1, sha2 0.11.1, base64 0.23.1 |
| 서버 HTTP와 설정 | axum 0.8.9, hyper 1.12.0, hyper-util 0.1.21, http-body-util 0.1.5, toml 1.1.8 |
| 기기 인증 | getrandom 0.4.3, subtle 2.6.1 |
| 서버 파일 접근과 이름 검사 | rustix 1.1.5, unicode-normalization 0.1.26, caseless 0.2.2 |
| 초안 및 서버 상태 저장 | SQLx 0.9.0, Tokio 1.54.1 |
| 앱 | Tauri 2.12.5, tauri-build 2.7.4, Tauri JS/CLI 2.12.4 |
| UI | React 19.3.1, TypeScript 7.0.2, Vite 8.3.5, CodeMirror state 6.7.6 / view 6.43.15 / commands 6.11.1 / markdown 6.5.2 |
| 검사 | Vitest 5.0.5, ESLint 10.13.0, typescript-eslint 8.72.0 |
| Android | Gradle 9.6.1, Android Gradle Plugin 9.3.1, Kotlin 2.2.10, NDK 30.0.16248370 |

잠긴 의존성의 이름, 버전과 라이선스 metadata는 다음 명령으로 재생성한다.

```bash
source scripts/dev-env.sh
mkdir -p .artifacts
python3 scripts/dependency-inventory.py > .artifacts/dependencies.json
```

서버 HTTP와 인증 의존성을 추가한 뒤 재생성한 기록은 Rust 외부 package 454개, npm lockfile package entry 249개이며 라이선스 metadata가 없는 항목은 없었다. 대상 OS별 선택 의존성과 개발 도구도 포함하므로 앱에 모두 번들된 수가 아니다.

MIT, Apache-2.0, BSD, ISC, Unicode-3.0 등과 MPL-2.0 항목이 포함되어 있다. Metadata 조회는 배포 고지와 소스 제공 의무 검토를 대신하지 않는다. 시스템 WebKitGTK와 GTK, Android 템플릿 및 번들 자산도 공개 배포 시 별도로 확인한다. 추가 폰트나 외부 이미지는 내려받지 않았으며 데스크톱 아이콘 원본은 저장소의 SVG다.

프로젝트 자체의 라이선스 결정과 배포용 고지 파일은 아직 미완료다.
