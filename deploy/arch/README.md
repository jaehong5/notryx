# Arch 및 Omarchy용 시험 패키지

Ubuntu 24.04 x86_64에서 빌드한 Notryx 실행 파일을 Arch 패키지로 포장한다. [Tauri의 Debian 패키지 재사용 방식](https://v2.tauri.app/distribute/aur/#extracting-from-a-debian-package)을 따른다. AUR에 게시하지 않으며, Arch 전체 배포판의 지원이나 서명된 운영판 출시를 뜻하지 않는다.

2026-11-10에 Omarchy 4.5.0 x86_64에서 `makepkg` 생성, 공유 라이브러리 연결과 앱 메뉴 파일 검사를 통과했다. 사용자는 패키지 설치, 실행과 직접 시험한 기본 동작이 정상임을 확인했다. 키링 복구, 물리 한글 입력과 업데이트 및 제거는 이 확인 범위에 포함하지 않는다.

## 생성

Arch 또는 Omarchy의 일반 사용자 계정에서 `makepkg`, `bsdtar`, `fakeroot`, `zstd`가 준비돼 있어야 한다. 저장소 루트 기준으로 임시 디렉터리를 만들고 패키지 정의와 이미 검증한 `.deb`를 복사한다.

```bash
notryx_package_dir=$(mktemp -d)
cp deploy/arch/PKGBUILD "$notryx_package_dir/"
cp target/release/bundle/deb/Notryx_0.1.0_amd64.deb "$notryx_package_dir/"
cd "$notryx_package_dir"
makepkg --noconfirm
pacman -Qip notryx-bin-0.1.0-1-x86_64.pkg.tar.zst
```

`makepkg`는 입력 `.deb`의 SHA-256을 검사한다. 이 정의는 지정한 개발 산출물 한 개에 고정돼 있다. 다른 산출물을 사용하려면 검증 후 해시를 갱신하며, 내용이 바뀐 패키지를 같은 버전으로 배포하지 않도록 `pkgrel`도 올린다. 체크섬 확인을 건너뛰지 않는다. 기본 명령은 시스템 패키지를 설치하거나 업데이트하지 않는다. 의존성이 없으면 중단하므로 필요한 도구와 실행 의존성을 먼저 준비해야 한다.

## 설치와 보존 범위

생성한 패키지를 설치할 PC에서 다음 명령을 실행한다.

```bash
sudo pacman -U ./notryx-bin-0.1.0-1-x86_64.pkg.tar.zst
notryx-client
```

패키지는 실행 파일, 앱 메뉴, 아이콘과 기존 고지를 보존한다. MIT 원문은 `/usr/share/licenses/notryx-bin/LICENSE`에도 포함한다. 별도 설치 및 제거 스크립트는 없다. 실행 파일은 다시 링크하거나 strip하지 않는다. 데이터 식별자 `org.notryx.m0`와 키링 조회 속성도 그대로다. 기기 데이터 위치와 업데이트 주의사항은 [Linux 클라이언트 안내](../../docs/linux-client.md#기존-데이터와-업데이트)를 따른다.

패키지 생성과 `pacman`의 메타데이터 조회는 실제 앱 실행 검증과 다르다. 설치 대상에서 공유 라이브러리 확인과 화면 실행, 저장 및 재시작, 키링 복구와 한글 입력을 별도로 확인해야 한다. Ubuntu에서 통과한 시험만으로 Omarchy의 이 항목들을 통과했다고 판단하지 않는다.
