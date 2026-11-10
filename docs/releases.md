# Linux 배포 파일 준비와 검증

현재는 `.deb`와 Arch `.pkg.tar.zst`의 수동 배포를 준비하는 단계다. 공식 릴리스 서명키와 공개 릴리스는 아직 없으며, 서명 검증 시험에는 합성 임시키를 사용한다. 아래 절차는 자동 업데이트나 M4 전체 게이트 통과를 뜻하지 않는다.

## 배포 파일 준비

같은 깨끗한 checkout에서 [Linux 패키지](linux-client.md#빌드와-검사)를 빌드하고 검사한 뒤 저장소 루트에서 실행한다.

```bash
source scripts/dev-env.sh
python3 scripts/prepare-release.py \
  --artifact target/release/bundle/deb/Notryx_0.1.0_amd64.deb \
  --output .artifacts/releases/0.1.0 \
  --profile linux-x86_64-package
```

Arch 파일도 배포하려면 [Arch 패키지 생성 안내](../deploy/arch/README.md)에 따라 같은 실행 파일로 만든 뒤 위 명령에 `--artifact /path/to/notryx-bin-0.1.0-1-x86_64.pkg.tar.zst`를 추가한다. `--artifact`는 여러 번 지정할 수 있다. 출력 디렉터리는 새 경로여야 하며 기존 파일을 덮어쓰지 않는다. `--allow-dirty`는 개발 시험용이고 공개 배포 준비에는 사용하지 않는다.

출력에는 패키지와 다음 파일이 들어간다.

| 파일 | 내용 |
|---|---|
| `release-manifest.json` | 버전, 각 파일의 이름, 종류, 크기와 SHA-256 |
| `sbom.cdx.json` | 범위를 명시한 CycloneDX 1.6 구성요소 목록 |
| `build-info.json` | 준비 시점의 Git revision, 작업 트리 상태, 도구 버전, lockfile과 설정 파일 해시, 지정한 시험 profile |

`source_revision`은 준비 명령을 실행한 checkout의 HEAD다. 입력 패키지가 그 커밋에서 빌드됐다는 증명은 아니다. 생성물에 이전 빌드가 섞이지 않도록 같은 checkout에서 새로 빌드한 파일을 사용하고 실제 빌드, 검사 및 CI 결과를 별도로 보관한다. `--profile`은 시험 조건의 이름이며 통과 결과를 생성하지 않는다.

SBOM은 Linux 클라이언트의 Rust 일반 및 빌드 의존성과 Vite 고지에 기록된 프런트엔드 번들 의존성을 포함한다. 실행 파일에 링크된 라이브러리만 분석한 결과는 아니며 운영체제 제공 라이브러리, 서버 이미지, Android와 프런트엔드 빌드 도구는 제외한다. 구성요소 metadata와 고지 수집은 최종 라이선스 감사를 대신하지 않는다. 형식은 [CycloneDX 1.6 JSON 명세](https://cyclonedx.org/docs/1.6/json/)를 따른다.

## 서명과 신뢰할 공개키

운영용 서명키가 준비된 뒤 배포 담당자가 manifest에 detached signature를 만든다. 아래 키 홈과 fingerprint는 실제로 승인한 값으로 바꾼다. 비밀키와 암호를 저장소, 배포 파일이나 명령행 인자에 넣지 않는다.

```bash
notryx_release_dir=.artifacts/releases/0.1.0
gpg --homedir /path/to/signing-home \
  --local-user RELEASE_SIGNING_FINGERPRINT --armor --detach-sign \
  --output "$notryx_release_dir/release-manifest.json.asc" \
  "$notryx_release_dir/release-manifest.json"
```

manifest 서명은 목록에 있는 패키지와 metadata의 해시를 보호한다. 서명 이후 파일을 바꾸면 다시 준비하고 서명해야 한다. 배포 디렉터리에는 목록에 있는 파일, `release-manifest.json`, `release-manifest.json.asc`만 둔다. 공개키와 별도 설명 파일을 이 디렉터리에 추가하지 않는다.

검증자는 이미 신뢰하는 경로에서 공개키의 전체 fingerprint를 대조하고 승인한 공개키만 별도 binary keyring에 보관한다. 내려받은 묶음의 공개키를 자동으로 신뢰하지 않는다. `gpgv`는 지정한 keyring의 모든 키를 신뢰하고 키 만료나 폐기를 검사하지 않으므로, 승인한 키의 상태와 keyring 갱신도 관리해야 한다. [GnuPG gpgv 안내](https://www.gnupg.org/documentation/manuals/gnupg/gpgv.html)

배포 담당자는 다음처럼 공개키만 내보낼 수 있다. 이 파일을 받았다는 사실만으로 신뢰가 성립하지는 않으며 검증자의 fingerprint 대조가 필요하다.

```bash
gpg --homedir /path/to/signing-home --export RELEASE_SIGNING_FINGERPRINT \
  > /path/to/notryx-release-public.gpg
```

## 검증 후 설치

신뢰하는 저장소 checkout의 검증 스크립트와 배포 디렉터리 밖에 보관한 공개 keyring을 사용한다. Python 3와 `gpgv`가 필요하다. 다음 Ubuntu 예시는 검증이 성공한 경우에만 설치 명령을 실행한다.

```bash
notryx_release_dir=/path/to/downloaded/notryx-0.1.0
python3 scripts/verify-release.py "$notryx_release_dir" \
  --keyring /path/to/trusted-notryx-keys.gpg --expected-version 0.1.0 && \
  sudo apt install "$notryx_release_dir/Notryx_0.1.0_amd64.deb"
```

Arch 및 Omarchy에서는 다음 설치 명령을 사용한다.

```bash
notryx_release_dir=/path/to/downloaded/notryx-0.1.0
python3 scripts/verify-release.py "$notryx_release_dir" \
  --keyring /path/to/trusted-notryx-keys.gpg --expected-version 0.1.0 && \
  sudo pacman -U "$notryx_release_dir/notryx-bin-0.1.0-1-x86_64.pkg.tar.zst"
```

검증 도구는 임시 GPG 홈에서 지정한 외부 keyring만 사용한다. manifest 크기는 1 MiB 이하로 제한하며 서명을 먼저 확인한 뒤 형식, 버전, 필수 파일 종류, 중복 이름과 경로를 검사한다. 누락, symlink, 목록에 없는 항목과 크기 또는 SHA-256 불일치는 실패한다. 도구 자체는 설치하거나 파일을 실행하지 않는다.

이 절차의 거절 검증과 OS 패키지 관리자의 정책은 구별한다. 사용자가 검증을 건너뛰고 직접 `apt` 또는 `pacman`을 실행하는 경우까지 이 도구가 차단하지는 않는다. 업데이트 전 앱 종료와 자료 보존은 [기존 데이터와 업데이트](linux-client.md#기존-데이터와-업데이트)를 따른다.

서명 검증의 자동 시험은 다음 명령으로 실행한다. 임시 홈의 합성 키와 합성 파일을 사용하며 공식 발행자 확인, 실제 패키지 설치나 전체 릴리스 게이트를 대신하지 않는다. 실행 결과와 GitHub Actions 결과는 각각 별도로 확인한다.

```bash
python3 scripts/test-release-verification.py
```
