#!/usr/bin/env python3
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from zipfile import ZipFile

SPEC = importlib.util.spec_from_file_location("prepare_drawio", Path(__file__).with_name("prepare-drawio.py"))
PREPARE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREPARE)


class DrawioPackagingTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        PREPARE.SOURCE = root / "source"
        PREPARE.CACHE = root / "cache"
        PREPARE.OUTPUT = root / "public" / "drawio"
        PREPARE.SOURCE.mkdir()
        PREPARE.CACHE.mkdir()
        (PREPARE.SOURCE / "licenses").mkdir()
        for name in ["PreConfig.js", "PostConfig.js", "bridge.js", "LICENSE.txt", "NOTICE.md"]:
            (PREPARE.SOURCE / name).write_text(name)
        archive = PREPARE.CACHE / "draw-v1.war"
        with ZipFile(archive, "w") as bundle:
            bundle.writestr("index.html", '<script src="js/bootstrap.js"></script>')
            bundle.writestr("js/bootstrap.js", "upstream bootstrap")
            bundle.writestr("js/main.js", "upstream main")
            bundle.writestr("WEB-INF/private.class", "server code")
            bundle.writestr("service-worker.js", "worker")
        self.manifest = {
            "version": "1", "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
            "url": "https://example.invalid/never-requested", "directories": [],
            "files": ["index.html", "js/bootstrap.js", "js/main.js"], "upstreamNotices": [],
        }
        self.write_manifest()

    def write_manifest(self):
        (PREPARE.SOURCE / "manifest.json").write_text(json.dumps(self.manifest))

    def test_offline_build_preserves_scripts_and_excludes_unselected_resources(self):
        PREPARE.prepare("desktop", True)
        self.assertEqual((PREPARE.OUTPUT / "js/main.js").read_text(), "upstream main")
        self.assertIn('src="js/notryx-bridge.js"', (PREPARE.OUTPUT / "index.html").read_text())
        self.assertFalse((PREPARE.OUTPUT / "WEB-INF").exists())
        self.assertFalse((PREPARE.OUTPUT / "service-worker.js").exists())
        self.assertEqual((PREPARE.OUTPUT / "LICENSE.txt").read_text(), "LICENSE.txt")

    def test_android_removes_previous_desktop_assets_without_fetching(self):
        PREPARE.prepare("desktop", True)
        self.manifest["sha256"] = "invalid"
        self.write_manifest()
        PREPARE.prepare("android", True)
        self.assertFalse(PREPARE.OUTPUT.exists())

    def test_corrupt_archive_does_not_replace_last_valid_bundle(self):
        PREPARE.prepare("desktop", True)
        (PREPARE.CACHE / "draw-v1.war").write_bytes(b"corrupt")
        with self.assertRaisesRegex(RuntimeError, "missing or corrupt"):
            PREPARE.prepare("desktop", True)
        self.assertEqual((PREPARE.OUTPUT / "js/main.js").read_text(), "upstream main")

    def test_missing_required_asset_fails_before_replacing_bundle(self):
        self.manifest["files"].append("js/missing.js")
        self.write_manifest()
        with self.assertRaisesRegex(RuntimeError, "Missing required"):
            PREPARE.prepare("desktop", True)
        self.assertFalse(PREPARE.OUTPUT.exists())


if __name__ == "__main__":
    unittest.main()
