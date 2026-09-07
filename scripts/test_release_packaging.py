"""Check release manifests and ZIP contents without building an executable."""

import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]


class ReleasePackages(unittest.TestCase):
    def package(self, root):
        script = root / "runner/scripts/package-release.sh"
        script.parent.mkdir(parents=True)
        shutil.copyfile(ROOT / "scripts/package-release.sh", script)
        result = subprocess.run(
            ["bash", str(script), str(root / "artifacts"), str(root / "out"),
             "r1-alpha7-nightly-fixture", "fixture-commit"],
            capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        out = root / "out"
        manifest = json.loads((out / "manifest.json").read_text())
        self.assertEqual(manifest["version"], "r1-alpha7")
        self.assertEqual(manifest["commit"], "fixture-commit")
        self.assertEqual(len(manifest["assets"]), 2)
        self.assertEqual({row["kind"] for row in manifest["assets"]}, {"engine", "runtime"})
        sums = dict((name.removeprefix("./"), digest) for digest, name in
                    (line.split() for line in (out / "SHA256SUMS").read_text().splitlines()))
        for row in manifest["assets"]:
            data = (out / row["name"]).read_bytes()
            self.assertEqual(row["size"], len(data))
            digest = hashlib.sha256(data).hexdigest()
            self.assertEqual(row["sha256"], digest)
            self.assertEqual(sums[row["name"]], digest)
        return out

    def test_desktop_layouts_keep_source_sdk_only_in_engine_package(self):
        for platform, prefix, suffix in (
            ("windows-x64", "", ".exe"),
            ("linux-x64", "Renzora.AppDir/", ""),
            ("macos-arm64", "Renzora.app/Contents/MacOS/", ""),
        ):
            with self.subTest(platform=platform), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                staged = root / "artifacts/build" / platform
                for name in (f"renzora{suffix}", f"renzora-editor{suffix}",
                             "rust-sdk/Cargo.toml", "plugins/example.data"):
                    path = staged / prefix / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text("fixture, not an executable")
                out = self.package(root)
                with zipfile.ZipFile(out / f"{platform}.zip") as archive:
                    self.assertIn(prefix + "rust-sdk/Cargo.toml", archive.namelist())
                    self.assertIn(prefix + f"renzora-editor{suffix}", archive.namelist())
                with zipfile.ZipFile(out / f"renzora-runtime-{platform}.zip") as archive:
                    self.assertIn(f"renzora{suffix}", archive.namelist())
                    self.assertIn("plugins/example.data", archive.namelist())
                    self.assertFalse(any("rust-sdk" in name or "renzora-editor" in name
                                         for name in archive.namelist()))

    def test_web_template_excludes_editor_and_has_matching_checksums(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            staged = root / "artifacts/build/web-wasm32"
            staged.mkdir(parents=True)
            for name in ("renzora-runtime.js", "renzora-runtime_bg.wasm", "renzora-editor.js"):
                (staged / name).write_text("fixture")
            out = self.package(root)
            with zipfile.ZipFile(out / "renzora-runtime-web-wasm32.zip") as archive:
                self.assertEqual(set(archive.namelist()),
                                 {"renzora-runtime.js", "renzora-runtime_bg.wasm"})


if __name__ == "__main__":
    unittest.main()
