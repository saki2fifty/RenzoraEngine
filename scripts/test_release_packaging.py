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
    def run_packager(self, root, tag="r1-alpha7-nightly-fixture", commit="fixture-commit"):
        script = root / "runner/scripts/package-release.sh"
        script.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / "scripts/package-release.sh", script)
        return subprocess.run(
            ["bash", str(script), str(root / "artifacts"), str(root / "out"),
             tag, commit],
            capture_output=True, text=True, timeout=30,
        )

    def package(self, root, platforms=1):
        result = self.run_packager(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        out = root / "out"
        manifest = json.loads((out / "manifest.json").read_text())
        self.assertEqual(manifest["version"], "r1-alpha7")
        self.assertEqual(manifest["commit"], "fixture-commit")
        self.assertEqual(len(manifest["assets"]), 2 * platforms)
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

    def test_metadata_is_json_encoded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            staged = root / "artifacts/job/windows-x64"
            staged.mkdir(parents=True)
            (staged / "renzora.exe").write_text("fixture")
            tag = 'r1-quote"backslash\\newline\n'
            commit = 'fixture"\\\n'
            result = self.run_packager(root, tag=tag, commit=commit)
            self.assertEqual(result.returncode, 0, result.stderr)
            metadata = json.loads((root / "out/manifest.json").read_text())
            self.assertEqual(metadata["tag"], tag)
            self.assertEqual(metadata["version"], tag)
            self.assertEqual(metadata["commit"], commit)

    def test_source_archive_uses_requested_commit_not_head_or_dirty_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            repo = root / "runner"
            repo.mkdir()

            def git(*args):
                return subprocess.check_output(
                    ["git", "-C", str(repo), "-c", "user.name=Fixture",
                     "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", *args],
                    text=True, stderr=subprocess.DEVNULL,
                ).strip()

            git("init")
            source = repo / "source.txt"
            source.write_text("requested revision")
            git("add", "source.txt")
            git("commit", "-m", "fixture one")
            first = git("rev-parse", "HEAD")
            source.write_text("later revision")
            git("commit", "-am", "fixture two")
            source.write_text("dirty file")
            staged = root / "artifacts/job/windows-x64"
            staged.mkdir(parents=True)
            (staged / "renzora.exe").write_text("fixture")
            invalid = self.run_packager(root, commit="missing-revision")
            self.assertNotEqual(invalid.returncode, 0)
            self.assertIn("source commit is not available locally", invalid.stderr)
            self.assertEqual(list((root / "out").iterdir()), [])
            result = self.run_packager(root, commit=first)
            self.assertEqual(result.returncode, 0, result.stderr)
            with zipfile.ZipFile(root / "out/engine-source.zip") as archive:
                self.assertEqual(archive.read("source.txt"), b"requested revision")
            metadata = json.loads((root / "out/manifest.json").read_text())
            self.assertEqual(metadata["commit"], first)
            row = next(row for row in metadata["assets"] if row["kind"] == "source")
            data = (root / "out" / row["name"]).read_bytes()
            self.assertEqual(row["sha256"], hashlib.sha256(data).hexdigest())
            self.assertEqual(row["size"], len(data))

    def test_existing_output_is_rejected_without_modifying_it(self):
        for name in ("old.zip", ".hidden", "manifest.json"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                out = root / "out"
                out.mkdir()
                sentinel = out / name
                sentinel.write_bytes(b"previous release")
                result = self.run_packager(root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("output directory must be empty", result.stderr)
                self.assertEqual(list(out.iterdir()), [sentinel])
                self.assertEqual(sentinel.read_bytes(), b"previous release")

    def test_duplicate_platforms_fail_before_any_archive_is_written(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for job in ("a", "b"):
                staged = root / "artifacts" / job / "windows-x64"
                staged.mkdir(parents=True)
                (staged / "renzora.exe").write_text(job)
            result = self.run_packager(root)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("duplicate platform", result.stderr)
            self.assertEqual(list((root / "out").iterdir()), [])
            for job in ("a", "b"):
                self.assertEqual((root / "artifacts" / job / "windows-x64/renzora.exe").read_text(), job)

    def test_distinct_platforms_package_together(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for platform in ("windows-x64", "windows-arm64"):
                staged = root / "artifacts" / platform / platform
                staged.mkdir(parents=True)
                (staged / "renzora.exe").write_text(platform)
                (staged / "renzora-editor.exe").write_text(platform)
            out = self.package(root, platforms=2)
            self.assertEqual(len(list(out.glob("*.zip"))), 4)

    def test_unrecognized_input_fails_without_packages(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "artifacts/job/unknown").mkdir(parents=True)
            result = self.run_packager(root)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("no recognised platform", result.stderr)
            self.assertEqual(list((root / "out").iterdir()), [])

    def test_incomplete_runtime_inputs_fail_before_any_platform_is_packaged(self):
        for platform, files in (
            ("windows-x64", {}),
            ("windows-x64", {"renzora.exe": ""}),
            ("windows-x64", {"renzora": "wrong platform"}),
            ("linux-x64", {"renzora.exe": "wrong platform"}),
            ("macos-arm64", {"Renzora.app/Contents/MacOS/renzora": ""}),
            ("web-wasm32", {"renzora-runtime.js": "fixture"}),
            ("web-wasm32", {"renzora-runtime_bg.wasm": "fixture"}),
            ("web-wasm32", {"renzora-runtime.js": "fixture", "renzora-runtime_bg.wasm": ""}),
        ):
            with self.subTest(platform=platform, files=files), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                good = root / "artifacts/a/windows-arm64"
                good.mkdir(parents=True)
                (good / "renzora.exe").write_text("valid earlier input")
                staged = root / "artifacts/z" / platform
                staged.mkdir(parents=True)
                for name, contents in files.items():
                    path = staged / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(contents)
                result = self.run_packager(root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("missing or empty runtime", result.stderr)
                self.assertEqual(list((root / "out").iterdir()), [])

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
                             "rust-sdk/Cargo.toml", "plugins/example.data",
                             "sdk/old.rmeta", "sdk.tar.zst", "librenzora_dylib.so",
                             "plugins/librenzora_dylib.so"):
                    path = staged / prefix / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text("fixture, not an executable")
                out = self.package(root)
                with zipfile.ZipFile(out / f"{platform}.zip") as archive:
                    self.assertIn(prefix + "rust-sdk/Cargo.toml", archive.namelist())
                    self.assertIn(prefix + f"renzora-editor{suffix}", archive.namelist())
                    self.assertIn(prefix + "plugins/librenzora_dylib.so", archive.namelist())
                    for obsolete in ("sdk/old.rmeta", "sdk.tar.zst", "librenzora_dylib.so"):
                        self.assertNotIn(prefix + obsolete, archive.namelist())
                        self.assertTrue((staged / prefix / obsolete).is_file())
                with zipfile.ZipFile(out / f"renzora-runtime-{platform}.zip") as archive:
                    self.assertIn(f"renzora{suffix}", archive.namelist())
                    if not suffix:
                        self.assertNotEqual(archive.getinfo("renzora").external_attr >> 16 & 0o111, 0)
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
