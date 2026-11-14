#!/usr/bin/env python3
"""Exercise target-specific Rust notice output without compiling the client."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("prepare-rust-notices.py")
spec = importlib.util.spec_from_file_location("rust_notices", SCRIPT)
notices = importlib.util.module_from_spec(spec)
spec.loader.exec_module(notices)


class RustNoticesTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="notryx-rust-notices-")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.license = "Copyright © 2026 Synthetic Authors\nPermission: 보존 예시\n"
        (self.directory / "LICENSE").write_text(self.license, encoding="utf-8")
        self.metadata = {
            "packages": [
                {"id": "client", "name": "notryx-client", "source": None},
                {"id": "library", "name": "synthetic-library", "version": "1.0.0",
                 "source": "registry+https://example.invalid/index", "license": "MIT",
                 "license_file": None, "manifest_path": str(self.directory / "Cargo.toml")},
            ],
            "resolve": {"nodes": [
                {"id": "client", "deps": [{"pkg": "library", "dep_kinds": [{"kind": None}]}]},
                {"id": "library", "deps": []},
            ]},
        }

    def test_default_render_keeps_linux_target(self):
        rendered = notices.render(self.metadata)
        self.assertIn("# Notryx Linux 클라이언트 Rust 의존성 고지", rendered)
        self.assertIn("`x86_64-unknown-linux-gnu`", rendered)

    def test_android_render_identifies_target_and_preserves_notice_text(self):
        rendered = notices.render(self.metadata, "aarch64-linux-android")
        self.assertIn("# Notryx Android 클라이언트 Rust 의존성 고지", rendered)
        self.assertIn("`aarch64-linux-android`", rendered)
        self.assertNotIn("Linux", rendered)
        for line in self.license.splitlines():
            self.assertIn("    " + line, rendered)

    def test_cli_writes_selected_target_from_metadata(self):
        metadata_path = self.directory / "metadata.json"
        metadata_path.write_text(json.dumps(self.metadata), encoding="utf-8")
        output = self.directory / "assets/rust-notices.md"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--metadata", str(metadata_path),
             "--target", "aarch64-linux-android", "--output", str(output)],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("# Notryx Android 클라이언트", output.read_text(encoding="utf-8"))
        self.assertIn("Copyright © 2026 Synthetic Authors", output.read_text(encoding="utf-8"))

    def test_cli_default_outputs_keep_linux_and_android_notices_separate(self):
        metadata_path = self.directory / "metadata.json"
        metadata_path.write_text(json.dumps(self.metadata), encoding="utf-8")
        arguments = [str(SCRIPT), "--metadata", str(metadata_path)]
        with patch.object(notices, "ROOT", self.directory):
            with patch.object(sys, "argv", arguments):
                notices.main()
            linux_output = self.directory / ".artifacts/linux-package/rust-notices.md"
            linux_contents = linux_output.read_bytes()
            self.assertIn(b"# Notryx Linux", linux_contents)

            with patch.object(sys, "argv", arguments + ["--target", "aarch64-linux-android"]):
                notices.main()
            self.assertEqual(linux_output.read_bytes(), linux_contents,
                             "Android generation must preserve Linux notices")
            android_output = self.directory / ".artifacts/android-package/rust-notices.md"
            self.assertIn("# Notryx Android", android_output.read_text(encoding="utf-8"))

    def test_cli_rejects_unsupported_target_before_writing(self):
        output = self.directory / "unsupported.md"
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--target", "unsupported-target", "--output", str(output)],
            capture_output=True, text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
