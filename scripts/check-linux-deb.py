#!/usr/bin/env python3
"""Inspect a locally built Linux package before installation testing."""

import argparse
import configparser
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("package", type=Path)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    package = args.package.resolve(strict=True)

    def field(name):
        return subprocess.check_output(["dpkg-deb", "-f", str(package), name], text=True).strip()

    if field("Package") != "notryx" or field("Architecture") != "amd64":
        raise SystemExit("Expected an amd64 Notryx package")
    depends = field("Depends")
    for dependency in ("libc6 (>= 2.39)", "libgtk-3-0", "libwebkit2gtk-4.1-0", "ca-certificates", "xdg-utils"):
        if dependency not in depends:
            raise SystemExit(f"Missing runtime dependency: {dependency}")

    with tempfile.TemporaryDirectory(prefix="notryx-deb-check-") as temporary:
        root = Path(temporary)
        subprocess.run(["dpkg-deb", "-x", str(package), str(root)], check=True)
        subprocess.run(["dpkg-deb", "-e", str(package), str(root / "DEBIAN")], check=True)
        for hook in ("preinst", "postinst", "prerm", "postrm"):
            if (root / "DEBIAN" / hook).exists():
                raise SystemExit(f"Unexpected package lifecycle script: {hook}")
        executable = root / "usr/bin/notryx-client"
        if not executable.is_file() or not executable.stat().st_mode & 0o111:
            raise SystemExit("Packaged client executable is missing")
        entries = list((root / "usr/share/applications").glob("*.desktop"))
        if len(entries) != 1:
            raise SystemExit("Expected one desktop entry")
        desktop = configparser.ConfigParser(interpolation=None)
        desktop.read(entries[0])
        entry = desktop["Desktop Entry"]
        if entry["Name"] != "Notryx" or entry["Exec"] != "notryx-client":
            raise SystemExit("Desktop name or launch command is incorrect")
        if not list((root / "usr/share/icons").glob(f"**/{entry['Icon']}.png")):
            raise SystemExit("Desktop icon is missing")
        notices = {
            "copyright": repo / "LICENSE",
            "third-party-licenses.md": repo / "apps/client/dist/third-party-licenses.md",
            "third-party-sources.md": repo / "apps/client/dist/third-party-sources.md",
            "rust-notices.md": repo / ".artifacts/linux-package/rust-notices.md",
        }
        for name, source in notices.items():
            if (root / "usr/share/doc/notryx" / name).read_bytes() != source.read_bytes():
                raise SystemExit(f"Packaged notice differs from its source: {name}")
        for path in root.rglob("*"):
            relative = path.relative_to(root)
            if relative.parts[0] not in ("usr", "DEBIAN"):
                raise SystemExit("Package contains files outside the application install paths")
            if "drawio" in path.name.lower() or path.name.endswith((".sqlite3", ".log")):
                raise SystemExit("Package contains retired assets or runtime data")
    print("Linux deb checks passed: metadata, launcher, icon, notices and install paths.")


if __name__ == "__main__":
    main()
