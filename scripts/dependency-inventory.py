#!/usr/bin/env python3
import json
from pathlib import Path
import subprocess


root = Path(__file__).resolve().parent.parent
metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version=1"], cwd=root, text=True))
rust = sorted(
    [{"name": p["name"], "version": p["version"], "license": p["license"]} for p in metadata["packages"] if p["source"]],
    key=lambda p: (p["name"], p["version"]),
)
lock = json.loads((root / "package-lock.json").read_text())
npm = sorted(
    [{"path": path, "version": item["version"], "license": item.get("license")} for path, item in lock["packages"].items() if "node_modules/" in path and not item.get("link")],
    key=lambda p: p["path"],
)
print(json.dumps({"scope": "locked dependency metadata; not a redistribution clearance", "rust": rust, "npm": npm}, indent=2))
