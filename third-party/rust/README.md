# Rust 의존성의 누락된 라이선스 원문

`scripts/prepare-rust-notices.py`는 잠긴 Linux 또는 Android 클라이언트 의존성의 고지를 Cargo 캐시에서 읽는다. 기본 대상은 Linux이며 Android는 `--target aarch64-linux-android`로 선택한다. crate에 원문 파일이 없는 아래 항목만 이 디렉터리에서 보충한다. `manifest.json`은 허용 버전, crate에 기록된 upstream commit, 원문 URL과 SHA-256을 고정한다. 버전이나 commit이 바뀌면 자동으로 새 버전에 적용하지 않는다.

- `alloc-stdlib 0.3.0`: `rust-alloc-no-stdlib` 저장소의 BSD-3-Clause 원문.
- `dlopen2 0.8.2`, `dlopen2_derive 0.4.3`: 같은 upstream commit의 MIT 원문.
- `libappindicator-sys 0.9.0`: `libappindicator-rs`의 MIT 및 Apache-2.0 원문.
- `ts-rs 12.0.1`, `ts-rs-macros 12.0.1`: 같은 upstream commit의 MIT 원문.
- `selectors 0.38.0`: upstream `lib.rs`가 명시한 MPL-2.0의 Mozilla 공식 원문. upstream commit은 `572ecba2d1600e7c3d490586692a209faf703baa`다.
- `jni 0.22.4`, `jni-macros 0.22.4`, `jni-sys-macros 0.4.1`: 각 crate에 기록된 upstream commit의 MIT 및 Apache-2.0 원문.
- `ndk 0.9.0`, `ndk-sys 0.6.0+11769913`, `ndk-context 0.1.1`: 각 crate에 기록된 upstream commit의 MIT 및 Apache-2.0 원문.
- `rustls-platform-verifier-android 0.2.0`: crate에 기록된 upstream commit의 MIT 및 Apache-2.0 원문.

원문은 수정하지 않았다. 생성한 고지에는 각 crate의 버전 고정 소스 다운로드 URL을 포함한다. 이 수집 결과는 공개 배포의 최종 라이선스 감사를 대신하지 않는다.
