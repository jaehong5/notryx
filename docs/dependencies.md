# 개발 의존성 기록

2026-10-27의 `Cargo.lock`, `package-lock.json` 기준이다. Rust와 npm의 직접 의존성은 각 manifest에서 버전을 고정하고 전이 의존성은 lockfile로 고정한다. 현재 앱 버전은 `0.1.0`, 프로토콜 식별자는 `mdw-sync/1.2`다.

| 영역 | 주요 직접 의존성 |
|---|---|
| 순수 계약 | uuid 1.29.0, serde 1.0.229, serde_json 1.0.154, schemars 1.2.3, ts-rs 12.0.1, sha2 0.11.1, base64 0.23.1 |
| 서버 HTTP와 설정 | axum 0.8.9, hyper 1.12.0, hyper-util 0.1.21, http-body-util 0.1.5, toml 1.1.8 |
| 기기 인증 | getrandom 0.4.3, subtle 2.6.1 |
| 서버 파일 접근과 이름 검사 | rustix 1.1.5, unicode-normalization 0.1.26, caseless 0.2.2 |
| 초안 및 서버 상태 저장 | SQLx 0.9.0, Tokio 1.54.1 |
| 클라이언트 통신과 미리보기 | reqwest 0.13.5, image 0.25.10, open 5.4.5 |
| 앱 | Tauri 2.12.5, tauri-build 2.7.4, Tauri JS/CLI 2.12.4 |
| UI | React 19.3.1, TypeScript 7.0.2, Vite 8.3.5, CodeMirror state 6.7.6 / view 6.43.15 / commands 6.11.1 / markdown 6.5.2, react-markdown 10.1.0, remark-gfm 4.0.1 |
| 검사 | Vitest 5.0.5, ESLint 10.13.0, typescript-eslint 8.72.0 |
| Android | Gradle 9.6.1, Android Gradle Plugin 9.3.1, Kotlin 2.2.10, NDK 30.0.16248370 |

잠긴 의존성의 이름, 버전과 라이선스 metadata는 다음 명령으로 재생성한다.

```bash
source scripts/dev-env.sh
mkdir -p .artifacts
python3 scripts/dependency-inventory.py > .artifacts/dependencies.json
```

클라이언트 통신과 미리보기 의존성을 추가한 뒤 재생성한 기록은 Rust 외부 package 510개, npm lockfile package entry 347개이며 라이선스 metadata가 없는 항목은 없었다. 대상 OS별 선택 의존성과 개발 도구도 포함하므로 앱에 모두 번들된 수가 아니다.

MIT, Apache-2.0, BSD, ISC, Unicode-3.0 등과 MPL-2.0 항목이 포함되어 있다. Metadata 조회는 배포 고지와 소스 제공 의무 검토를 대신하지 않는다. 시스템 WebKitGTK와 GTK, Android 템플릿 및 번들 자산도 공개 배포 시 별도로 확인한다. 추가 폰트나 외부 이미지는 내려받지 않았으며 데스크톱 아이콘 원본은 저장소의 SVG다.

Notryx 자체 코드와 문서는 [MIT 라이선스](../LICENSE)를 적용한다. 외부 라이브러리, 폰트, 아이콘과 번들 자산은 각각의 원래 라이선스를 유지하며 MIT로 재라이선스하지 않는다. draw.io 편집기 번들은 현재 배포에서 제외하며, 장기 후속 선택 플러그인으로 도입할 때 라이선스와 자산별 조건을 다시 검토한다. 전체 배포물의 고지와 소스 제공 의무 검토는 아직 미완료다.

Linux `.deb`는 `/usr/share/doc/notryx/`에 MIT 원문, Vite가 생성한 프런트엔드 고지 및 소스 안내, Rust 의존성 고지를 포함한다. `scripts/prepare-rust-notices.py`가 잠긴 Linux 클라이언트의 일반 및 빌드 의존성을 수집한다. crate에 빠진 원문만 [고정된 보충 파일](../third-party/rust/README.md)에서 읽고 버전, commit과 해시를 확인한다. 생성 결과는 빌드 도구도 포함하며 최종 배포 감사를 대신하지 않는다. [패키지 검사](linux-client.md#패키지-검증)는 설치된 고지와 생성 원문의 일치도 확인한다.

배포 준비 시에는 [Linux 배포 SBOM 안내](releases.md#배포-파일-준비)에 따라 `sbom.cdx.json`을 생성한다. 위 개발 의존성 전체 목록과 달리 Linux 클라이언트의 Rust 일반 및 빌드 의존성과 프런트엔드 번들 고지의 구성요소를 수집한다. 운영체제 라이브러리, 서버 이미지, Android와 프런트엔드 빌드 도구는 제외하며, 입력 패키지의 빌드 출처나 전체 라이선스 적합성을 증명하지 않는다.
