#!/usr/bin/env python3
import subprocess
import tomllib
from pathlib import Path


root = Path(__file__).resolve().parent.parent
workspace = tomllib.loads((root / "Cargo.toml").read_text())
for member in workspace["workspace"]["members"]:
    manifest = tomllib.loads((root / member / "Cargo.toml").read_text())
    name = manifest["package"]["name"]
    if name not in {"notryx-domain", "notryx-protocol", "notryx-sync-engine"}:
        continue
    tree = subprocess.check_output(
        ["cargo", "tree", "--locked", "--edges", "normal", "--prefix", "none", "-p", name],
        cwd=root, text=True,
    )
    forbidden = {"axum", "tokio", "hyper", "reqwest", "tauri", "wry", "gtk", "sqlx", "sqlx-core", "sqlx-sqlite", "rusqlite"}
    actual = {line.split()[0] for line in tree.splitlines() if line.strip()}
    bad = actual & forbidden
    if bad:
        raise SystemExit(f"{name}: forbidden runtime dependencies: {sorted(bad)}")
    print(f"{name}: pure dependency boundary passed")
