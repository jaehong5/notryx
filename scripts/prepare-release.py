#!/usr/bin/env python3
"""Prepare unsigned Linux client release files for detached signing."""

import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib
from urllib.parse import quote
import uuid


ROOT = Path(__file__).resolve().parent.parent
SAFE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]*\Z")
RESERVED = {"release-manifest.json", "release-manifest.json.asc", "sbom.cdx.json", "build-info.json"}


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def component(ecosystem, name, version, license_name, scope):
    purl = f"pkg:{ecosystem}/{quote(name, safe='/')}@{quote(version, safe='')}"
    value = {
        "type": "library", "name": name, "version": version,
        "bom-ref": purl, "purl": purl,
        "properties": [{"name": "notryx:inventory-scope", "value": scope}],
    }
    if license_name:
        value["licenses"] = ([{"license": {"name": license_name}}] if "/" in license_name
                             else [{"expression": license_name}])
    return value


def sbom(version, timestamp):
    spec = importlib.util.spec_from_file_location("rust_notices", ROOT / "scripts/prepare-rust-notices.py")
    notices = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(notices)
    metadata = json.loads(command("cargo", "metadata", "--locked", "--offline", "--filter-platform",
                                  notices.TARGET, "--format-version", "1"))
    components = [component("cargo", package["name"], package["version"], package["license"],
                            "Linux client normal and build dependencies; not a linked-library inventory")
                  for package in notices.dependency_packages(metadata)]
    lock = json.loads((ROOT / "package-lock.json").read_text())
    npm_packages = {
        (name.rsplit("node_modules/", 1)[-1], item["version"])
        for name, item in lock["packages"].items()
        if "node_modules/" in name and not item.get("link")
    }
    frontend_notices = ROOT / "apps/client/dist/third-party-licenses.md"
    bundled = re.findall(r"^## (.+) - (\S+) \((.+)\)$", frontend_notices.read_text(), re.MULTILINE)
    if not bundled:
        raise ValueError("build the frontend before preparing its dependency inventory")
    for name, package_version, license_name in bundled:
        if (name, package_version) not in npm_packages:
            raise ValueError(f"bundled dependency is absent from the lockfile: {name}@{package_version}")
        components.append(component("npm", name, package_version, license_name,
                                    "Frontend dependencies listed by the Vite bundle license report"))
    components = sorted({value["bom-ref"]: value for value in components}.values(), key=lambda value: value["bom-ref"])
    return {
        "$schema": "http://cyclonedx.org/schema/bom-1.6.schema.json",
        "bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1,
        "serialNumber": f"urn:uuid:{uuid.uuid4()}",
        "metadata": {
            "timestamp": timestamp,
            "component": {"type": "application", "name": "Notryx Linux client", "version": version},
            "properties": [{"name": "notryx:inventory-scope", "value":
                "Linux Rust normal/build dependencies and bundled frontend packages. "
                "Excludes system-provided libraries, server images, Android and frontend build tools. "
                "Package metadata and notices describe the external runtime dependencies."}],
        },
        "components": components,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", type=Path, action="append", required=True)
    parser.add_argument("--output", type=Path, required=True, help="A new output directory")
    parser.add_argument("--profile", required=True, help="Applied test profile; not a claim that its checks passed")
    parser.add_argument("--allow-dirty", action="store_true", help="Prepare development metadata and record the dirty tree")
    args = parser.parse_args()
    source_revision = command("git", "rev-parse", "HEAD")
    dirty = bool(command("git", "status", "--porcelain"))
    if dirty and not args.allow_dirty:
        raise ValueError("commit or isolate source changes before release preparation; use --allow-dirty only for development")
    artifacts = []
    names = set(RESERVED)
    for artifact in args.artifact:
        if not artifact.is_file() or artifact.is_symlink():
            raise ValueError("artifacts must be regular files")
        if not SAFE_NAME.fullmatch(artifact.name) or artifact.name in names:
            raise ValueError("artifact names must be unique safe filenames")
        if not artifact.name.endswith((".deb", ".pkg.tar.zst")):
            raise ValueError("only Linux .deb and Arch .pkg.tar.zst packages are supported")
        names.add(artifact.name)
        artifacts.append(artifact.resolve())
    output = args.output.resolve()
    if output.exists():
        raise ValueError("the output directory already exists; prepared files are never overwritten")
    version = json.loads((ROOT / "package.json").read_text())["version"]
    cargo_version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    tauri_version = json.loads((ROOT / "apps/client/src-tauri/tauri.conf.json").read_text())["version"]
    if not version == cargo_version == tauri_version:
        raise ValueError("application versions disagree")
    for artifact in artifacts:
        if artifact.name.endswith(".deb") and command("dpkg-deb", "-f", str(artifact), "Version") != version:
            raise ValueError("Debian package version differs from the source version")
        if artifact.name.endswith(".pkg.tar.zst"):
            metadata = command("tar", "-xOf", str(artifact), ".PKGINFO")
            package_versions = re.findall(r"^pkgver = (.+)$", metadata, re.MULTILINE)
            if len(package_versions) != 1 or package_versions[0].rpartition("-")[0] != version:
                raise ValueError("Arch package version differs from the source version")
    timestamp = datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    inventory = sbom(version, timestamp)
    build_info = {
        "source_revision": source_revision, "source_dirty": dirty,
        "prepared_at": timestamp, "target": "x86_64-unknown-linux-gnu",
        "test_profile": args.profile,
        "test_results": "Record executed results separately; a profile name does not prove a passed check.",
        "tools": {name: command(name, "--version") for name in ("rustc", "cargo", "node", "npm")},
        "source_files": {name: digest(ROOT / name) for name in (
            "Cargo.lock", "package-lock.json", "rust-toolchain.toml", "apps/client/src-tauri/tauri.conf.json",
            "apps/client/src-tauri/tauri.linux.conf.json")},
    }
    output.mkdir(parents=True)
    for artifact in artifacts:
        shutil.copyfile(artifact, output / artifact.name)
    write_json(output / "sbom.cdx.json", inventory)
    write_json(output / "build-info.json", build_info)
    files = [(artifact.name, "package") for artifact in artifacts]
    files += [("sbom.cdx.json", "sbom"), ("build-info.json", "build-info")]
    manifest = {
        "format": "notryx-release/1", "version": version,
        "files": [{"name": name, "kind": kind, "size": (output / name).stat().st_size,
                   "sha256": digest(output / name)} for name, kind in files],
    }
    write_json(output / "release-manifest.json", manifest)
    print(f"Prepared {len(artifacts)} package(s) and {len(inventory['components'])} dependency entries. "
          "The manifest is unsigned and must be signed and verified before distribution.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"Could not prepare release files: {error}", file=sys.stderr)
        sys.exit(1)
