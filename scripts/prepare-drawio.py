#!/usr/bin/env python3
"""Prepare the pinned desktop editor assets; exclude them from Android builds."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import tempfile
import urllib.request
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "third-party" / "drawio"
OUTPUT = ROOT / "apps" / "client" / "public" / "drawio"
CACHE = ROOT / ".local" / "drawio"


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def prepare(target, offline):
    if target == "android":
        if OUTPUT.exists():
            shutil.rmtree(OUTPUT)
        print("draw.io editor assets excluded from Android")
        return
    manifest = json.loads((SOURCE / "manifest.json").read_text())
    CACHE.mkdir(parents=True, exist_ok=True)
    archive = CACHE / f"draw-v{manifest['version']}.war"
    if not archive.exists() or sha256(archive) != manifest["sha256"]:
        if offline:
            raise RuntimeError("Pinned draw.io archive is missing or corrupt; run without --offline once")
        with tempfile.NamedTemporaryFile(dir=CACHE, delete=False) as stream:
            pending = Path(stream.name)
            try:
                with urllib.request.urlopen(manifest["url"], timeout=60) as response:
                    shutil.copyfileobj(response, stream)
                stream.close()
                if sha256(pending) != manifest["sha256"]:
                    raise RuntimeError("draw.io archive SHA-256 mismatch")
                pending.replace(archive)
            finally:
                pending.unlink(missing_ok=True)
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=OUTPUT.parent, prefix=".drawio-") as temporary:
        staged = Path(temporary)
        with ZipFile(archive) as bundle:
            for entry in bundle.infolist():
                name = entry.filename
                if entry.is_dir() or not (name in manifest["files"] or
                        name in manifest["upstreamNotices"] or
                        any(name.startswith(prefix) for prefix in manifest["directories"])):
                    continue
                path = PurePosixPath(name)
                if path.is_absolute() or ".." in path.parts or entry.file_size > 32 * 1024 * 1024:
                    raise RuntimeError("Invalid draw.io archive entry")
                destination = staged.joinpath(*path.parts)
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(bundle.read(entry))
        for name in manifest["files"]:
            if not staged.joinpath(name).is_file():
                raise RuntimeError(f"Missing required draw.io asset: {name}")
        html = (staged / "index.html").read_text()
        marker = '<script src="js/bootstrap.js"></script>'
        if html.count(marker) != 1:
            raise RuntimeError("draw.io bootstrap entry changed")
        html = html.replace(marker, marker + '\n<script src="js/notryx-bridge.js"></script>')
        (staged / "index.html").write_text(html)
        for source, destination in [
            ("PreConfig.js", "js/PreConfig.js"),
            ("PostConfig.js", "js/PostConfig.js"),
            ("bridge.js", "js/notryx-bridge.js"),
            ("LICENSE.txt", "LICENSE.txt"),
            ("NOTICE.md", "NOTICE.md"),
            ("manifest.json", "notryx-manifest.json"),
        ]:
            shutil.copyfile(SOURCE / source, staged / destination)
        shutil.copytree(SOURCE / "licenses", staged / "licenses")
        if OUTPUT.exists():
            shutil.rmtree(OUTPUT)
        staged.rename(OUTPUT)
    files = list(OUTPUT.rglob("*"))
    size = sum(path.stat().st_size for path in files if path.is_file())
    print(f"draw.io {manifest['version']}: {size:,} bytes prepared from verified archive")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=["desktop", "android"], default=(
        "android" if os.environ.get("TAURI_ENV_PLATFORM", "").startswith("android") or
        "android" in os.environ.get("TAURI_ENV_TARGET_TRIPLE", "") else "desktop"))
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    prepare(args.target, args.offline)
