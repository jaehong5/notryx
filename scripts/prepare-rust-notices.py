#!/usr/bin/env python3
"""Collect locked client dependency notices for Linux or Android packaging."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
TARGET = "x86_64-unknown-linux-gnu"
TARGET_LABELS = {TARGET: "Linux", "aarch64-linux-android": "Android"}
FALLBACK = ROOT / "third-party/rust"
NOTICE_NAME = re.compile(r"^(licen[sc]e|copying|copyright|notice)(?:$|[._-])", re.I)


def dependency_packages(metadata):
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    roots = [package["id"] for package in packages.values()
             if package["name"] == "notryx-client" and package["source"] is None]
    if len(roots) != 1:
        raise ValueError("expected one local notryx-client package")
    pending = roots[:]
    visited = set()
    while pending:
        package_id = pending.pop()
        if package_id in visited:
            continue
        visited.add(package_id)
        for dependency in nodes[package_id]["deps"]:
            if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                pending.append(dependency["pkg"])
    return sorted((packages[key] for key in visited if packages[key]["source"] is not None),
                  key=lambda package: (package["name"], package["version"]))


def package_notices(package, fallbacks):
    directory = Path(package["manifest_path"]).parent
    candidates = set()
    for path in directory.rglob("*"):
        if not path.is_file():
            continue
        relative = path.relative_to(directory)
        if NOTICE_NAME.match(path.name) or any(
                part.lower() in {"licenses", "licences"} for part in relative.parts[:-1]):
            candidates.add(path)
    if package["license_file"]:
        candidates.add(directory / package["license_file"])
    notices = [(str(path.relative_to(directory)), path.read_text(encoding="utf-8"))
               for path in sorted(candidates)]
    if notices:
        return notices
    key = f'{package["name"]}@{package["version"]}'
    fallback = fallbacks.get(key)
    if fallback is None:
        raise ValueError(f"no license text found for {key}")
    vcs = json.loads((directory / ".cargo_vcs_info.json").read_text(encoding="utf-8"))
    if vcs["git"]["sha1"] != fallback["commit"]:
        raise ValueError(f"fallback source commit mismatch for {key}")
    for entry in fallback["files"]:
        content = (FALLBACK / entry["path"]).read_bytes()
        if hashlib.sha256(content).hexdigest() != entry["sha256"]:
            raise ValueError(f"fallback notice checksum mismatch for {key}")
        notices.append((f'{entry["path"]} ({entry["url"]})', content.decode("utf-8")))
    return notices


def render(metadata, target=TARGET):
    fallbacks = json.loads((FALLBACK / "manifest.json").read_text(encoding="utf-8"))
    packages = dependency_packages(metadata)
    lines = [
        f"# Notryx {TARGET_LABELS[target]} 클라이언트 Rust 의존성 고지", "",
        f"대상: `{target}`. 잠긴 Cargo metadata에서 `notryx-client`의 일반 및 빌드 의존성을 수집했다.",
        "개발 전용 의존성은 제외한다. 빌드 도구도 포함하므로 아래 목록이 실행 파일에 링크된 구성요소 목록과 같지는 않다.",
        "이 문서는 고지 수집 결과이며 공개 배포의 최종 라이선스 감사를 대신하지 않는다.",
        "의존성 소스는 수정하지 않았다. 각 항목의 버전 고정 소스 링크에서 원문을 받을 수 있다.",
        "MPL-2.0 구성요소의 소스에도 같은 링크를 사용할 수 있으며 해당 소스의 라이선스는 유지된다.",
        "운영체제가 제공하는 동적 라이브러리의 고지는 설치된 시스템 패키지에서 별도로 제공한다.", "",
        f"외부 패키지: {len(packages)}개.", "",
    ]
    for package in packages:
        name, version = package["name"], package["version"]
        if not package["source"].startswith("registry+"):
            raise ValueError(f"unhandled dependency source for {name}@{version}")
        if not package["license"] and not package["license_file"]:
            raise ValueError(f"no license declaration for {name}@{version}")
        lines.extend([
            f"## {name} {version}", "",
            f'라이선스: `{package["license"] or "license-file"}`.', "",
            f"소스: https://static.crates.io/crates/{name}/{name}-{version}.crate", "",
        ])
        if package.get("authors"):
            lines.extend(["작성자: " + ", ".join(package["authors"]), ""])
        for filename, content in package_notices(package, fallbacks):
            lines.extend([f"### {filename}", ""])
            lines.extend("    " + line for line in content.splitlines())
            lines.append("")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGET_LABELS, default=TARGET,
                        help="Target platform for dependency selection and notice heading")
    parser.add_argument("--metadata", type=Path,
                        help="Use already filtered Cargo metadata instead of invoking Cargo")
    parser.add_argument("--output", type=Path,
                        default=ROOT / ".artifacts/linux-package/rust-notices.md")
    args = parser.parse_args()
    if args.metadata:
        metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
    else:
        result = subprocess.run(
            ["cargo", "metadata", "--locked", "--offline", "--filter-platform", args.target,
             "--format-version", "1"], cwd=ROOT, check=True, capture_output=True, text=True,
        )
        metadata = json.loads(result.stdout)
    contents = render(metadata, args.target)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(contents, encoding="utf-8")
    print(f"Rust notices prepared for {len(dependency_packages(metadata))} dependency packages.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"Could not prepare Rust notices: {error}", file=sys.stderr)
        sys.exit(1)
