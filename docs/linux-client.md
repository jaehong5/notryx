# Linux 클라이언트 설치 패키지

현재 패키지는 Ubuntu 24.04 x86_64에서 빌드하는 개발 시험판이다. Ubuntu의 `.deb` 설치 및 업데이트를 자동 시험했고, 같은 실행 파일로 만든 Arch 패키지는 Omarchy 4.5.0 x86_64에서 사용자가 설치, 실행과 기본 동작을 확인했다. M4 운영판 출시, 서명된 릴리스 또는 Android 지원을 뜻하지 않는다. 그 밖의 배포판과 ARM64는 아직 검증하지 않았다.

## 빌드와 검사

[개발 환경](development.md)을 준비한 Ubuntu 24.04 x86_64에서 실행한다.

```bash
source scripts/dev-env.sh
npm ci
CARGO_BUILD_JOBS=1 npm run tauri -- build --bundles deb -- --locked -j 1
python3 scripts/check-linux-deb.py target/release/bundle/deb/Notryx_0.1.0_amd64.deb
sha256sum target/release/bundle/deb/Notryx_0.1.0_amd64.deb
```

결과는 `target/release/bundle/deb/Notryx_0.1.0_amd64.deb`다. 빌드에는 Rust와 Node.js가 필요하지만 설치한 앱 실행에는 필요하지 않다. 패키지는 시스템의 GTK 3와 WebKitGTK 4.1을 사용하며 APT가 필요한 실행 의존성과 HTTPS용 CA 인증서를 설치한다. 빌드 기준에 맞춰 glibc 2.39 이상을 요구한다. 오래된 Linux에서 실행할 수 있다고 가정하지 않는다. [Tauri의 Linux 빌드 기준](https://v2.tauri.app/distribute/debian/#limitations)을 참고한다.

체크섬은 파일 동일성 확인용이며 발행자 인증을 대신하지 않는다. 서명 및 변조 거절 검증과 공개 릴리스 게시 절차는 후속 작업이다.

## 설치와 실행

앱을 실행할 Linux PC에서 직접 빌드한 패키지를 설치한다.

```bash
sudo apt install ./target/release/bundle/deb/Notryx_0.1.0_amd64.deb
notryx-client
```

다른 디렉터리에 패키지를 복사했다면 설치 명령의 파일 경로를 바꾼다. 데스크톱 앱 메뉴의 **Notryx**로도 실행할 수 있다. [기기 등록 안내](development.md#개발-서버-실행과-조회)에 따라 자체 호스팅 서버에 연결한다. 특정 외부 업체의 계정이나 터널을 요구하지 않는다.

인증정보를 재실행 후에도 복구하려면 로그인 세션에서 Secret Service를 제공하는 키링이 사용 가능해야 한다. 키링을 사용할 수 없을 때의 세션 전용 인증은 [개발 안내](development.md)를 참고한다.

### Arch 및 Omarchy

`.deb`를 직접 설치하지 않고 [Arch 패키지 생성 안내](../deploy/arch/README.md)에 따라 `.pkg.tar.zst` 파일을 준비한다. 설치와 실행은 다음과 같다.

```bash
sudo pacman -U ./notryx-bin-0.1.0-1-x86_64.pkg.tar.zst
notryx-client
```

현재 `PKGBUILD`는 검증한 개발 산출물의 SHA-256에 고정돼 있으며 AUR에 게시하지 않았다. 2026-11-10의 Omarchy 사용자 확인은 설치, 실행과 직접 시험한 기본 동작에 한정한다. 키링 복구, 물리 한글 입력과 전원 장애의 별도 검증을 대신하지 않는다.

## 기존 데이터와 업데이트

표시 이름은 `Notryx`이며 기존 앱 식별자 `org.notryx.m0`는 유지한다. 기본 기기 데이터 위치는 `${XDG_DATA_HOME:-$HOME/.local/share}/org.notryx.m0/`다. 초안, base, 전송 대기 자료, 등록 정보와 테마를 포함한다. 인증 비밀은 운영체제 키링에 저장한다. 패키지는 키링 조회 속성을 바꾸지 않는다.

개발 시험에서 `NOTRYX_M0_DATA_DIR`를 지정했다면 계속 같은 경로로 실행해야 그 자료를 사용한다. 앱 메뉴 실행은 별도 환경 설정이 없으면 기본 경로를 사용하므로, 다른 시험 경로의 자료를 자동으로 옮기거나 병합하지 않는다.

업데이트할 때는 앱을 닫고 새 `.deb` 파일에 같은 `sudo apt install ./파일명.deb` 명령을 사용한다. 기존 기기 데이터 경로를 유지하며 설치 스크립트에서 자료나 인증정보를 삭제하지 않는다. 데이터 형식을 변경한 버전에서 이전 버전으로 되돌리는 것은 이 안내의 검증 범위가 아니다. 별도 보관이 필요하면 앱을 닫은 뒤 기기 데이터 디렉터리를 복사한다. 서버 백업과 운영체제 키링 보관은 별개다.

Arch 패키지는 앱을 닫은 뒤 새 파일에 `sudo pacman -U ./파일명.pkg.tar.zst`를 실행한다. 앱만 제거할 때는 `sudo pacman -R notryx-bin`을 사용한다. 패키지의 설치 및 제거 경로에 기기 자료와 키링은 포함하지 않는다. Arch의 업데이트와 제거는 실제 사용자 시험으로 아직 확인하지 않았다.

앱만 제거하려면 다음 명령을 사용한다. 기기 데이터와 키링 항목은 남긴다.

```bash
sudo apt remove notryx
```

## 패키지 검증

패키지 내용 검사는 실행 파일, 앱 메뉴, 아이콘, 실행 의존성, 고지 원문 일치와 설치 경로를 확인한다. CI도 `.deb`를 만들고 추출한 바이너리로 기존 문서 편집 및 복구 시험을 실행한다.

다음 명령은 임시 Ubuntu 컨테이너에서 APT 설치, 낮은 버전 번호의 합성 패키지에서 업데이트, 제거 후 합성 사용자 파일 보존을 검사한다. 호스트에는 앱을 설치하지 않는다. Docker와 컨테이너 안의 APT 다운로드가 필요하다.

```bash
bash scripts/test-linux-deb-install.sh target/release/bundle/deb/Notryx_0.1.0_amd64.deb
```

기존 앱 실행 파일과 합성 서버 바이너리를 추가로 지정하면 합성 키링의 실제 앱 교체 시험도 실행한다. 첫 앱에서 기기를 등록하고 오프라인 초안을 저장한 뒤 앱을 닫고 설치판으로 다시 실행한다. 기본 데이터 경로의 초안, base, outbox와 등록 정보 보존, 키링 인증 복구, 재연결 후 기존 요청의 서버 반영을 확인한다.

```bash
bash scripts/test-linux-deb-install.sh target/release/bundle/deb/Notryx_0.1.0_amd64.deb \
  /path/to/previous/notryx-client target/debug/notryx-server
```

이 선택 시험은 WebKit의 중첩 namespace를 허용하기 위해 임시 컨테이너의 seccomp와 AppArmor 프로필을 해제한다. 호스트 보안 설정은 바꾸지 않으며 지정한 실행 파일과 시험 스크립트만 읽기 전용으로 마운트한다. 이 시험은 컨테이너 보안 프로필이나 실제 사용자 키링, 물리 한글 입력, 데스크톱 메뉴 클릭을 검증하지 않는다.

앱 교체 시험의 합성 서버와 기기 저장소는 `/work/tmp`의 tmpfs에 둔다. 프로세스 종료와 재실행 후 자료 보존을 확인하며 디스크 영속성이나 전원 장애를 검증하지는 않는다.

이번 교체 대상은 같은 데이터 스키마를 사용하는 직전 개발 앱이다. 데이터 스키마가 바뀌는 버전의 업그레이드는 별도 마이그레이션 검증이 필요하다.

## 라이선스와 남은 검증

`/usr/share/doc/notryx/`에 Notryx MIT 원문, 프런트엔드 의존성 고지와 소스 안내, Rust 의존성 고지를 설치한다. Rust 고지는 잠긴 Linux 클라이언트의 일반 및 빌드 의존성에서 생성한다. crate에 빠진 원문은 버전, upstream commit과 원문 해시를 고정한 파일로 보충하며 누락이나 불일치가 있으면 패키지 생성을 중단한다. 빌드 도구도 포함하므로 목록 전체가 실행 파일에 링크된 것은 아니다.

공개 배포의 최종 라이선스 감사, 릴리스 서명, 실제 사용자 키링과 현재 버전의 물리 한글 입력 검증은 남아 있다. 기존 편집기 성능 및 검색 진행 표시의 미통과 기록도 유지한다. 설치 패키지 준비를 전체 릴리스 게이트 통과로 해석하지 않는다.
