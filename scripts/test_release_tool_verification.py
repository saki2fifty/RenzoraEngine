"""Exercise the actual UPX install snippets with local, harmless archives."""

import hashlib
import io
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = (ROOT / ".github/workflows/build-engine.yml").read_text()
DOCKER = (ROOT / "docker/base/Dockerfile").read_text()


class VerifiedTool(unittest.TestCase):
    def test_version_and_amd64_digest_agree(self):
        for pattern in (r"UPX_VERSION=([\d.]+)", r"UPX_SHA256=([0-9a-f]{64})"):
            self.assertEqual(re.search(pattern, WORKFLOW)[1], re.search(pattern, DOCKER)[1])
        self.assertEqual(len(re.findall(r"UPX_SHA256=[0-9a-f]{64}", DOCKER)), 2)

    def run_install(self, lane, machine, corrupt=False):
        version = re.search(r"UPX_VERSION=([\d.]+)", DOCKER)[1]
        arch = "arm64_linux" if machine == "aarch64" else "amd64_linux"
        if lane == "workflow":
            block = WORKFLOW.split("          UPX_VERSION=", 1)[1].split("          while ", 1)[0]
            block = "UPX_VERSION=" + textwrap.dedent(block)
        else:
            block = DOCKER.split("RUN case ", 1)[1].split("\n\n", 1)[0]
            block = "case " + block.replace("\\\n", " ")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "fixture.tar.xz"
            with tarfile.open(archive, "w:xz") as stream:
                content = b"harmless test payload, never executed\n"
                entry = tarfile.TarInfo(f"upx-{version}-{arch}/upx")
                entry.size = len(content)
                stream.addfile(entry, io.BytesIO(content))
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            block = re.sub(r"UPX_SHA256=[0-9a-f]{64}", "UPX_SHA256=" + digest, block)
            if corrupt:
                archive.write_bytes(b"corrupted download")
            bin_dir = root / "bin"
            bin_dir.mkdir()
            # Replace only I/O destinations. The real checksum and shell failure
            # propagation remain unchanged, and tar runs only after verification.
            block = block.replace("/tmp", str(root)).replace("/usr/local/bin", str(bin_dir))
            shims = {
                "curl": '#!/bin/sh\nwhile [ "$1" != "-o" ]; do shift; done\ncp "$TEST_ARCHIVE" "$2"\n',
                "uname": '#!/bin/sh\necho "$TEST_MACHINE"\n',
                "tar": '#!/bin/sh\ntouch "$TEST_EXTRACTED"\nexec ' + shutil.which("tar") + ' "$@"\n',
            }
            for name, content in shims.items():
                path = bin_dir / name
                path.write_text(content)
                path.chmod(0o755)
            marker = root / "extracted"
            env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}",
                       TEST_ARCHIVE=str(archive), TEST_MACHINE=machine,
                       TEST_EXTRACTED=str(marker), UPX_VERSION=version)
            result = subprocess.run(["bash", "-euc", block], env=env,
                                    capture_output=True, text=True, timeout=10)
            if corrupt or machine == "unsupported":
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(marker.exists(), "must reject before extraction")
            else:
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertTrue(marker.exists())

    def test_workflow_accepts_verified_archive(self):
        self.run_install("workflow", "x86_64")

    def test_workflow_rejects_corrupted_archive(self):
        self.run_install("workflow", "x86_64", corrupt=True)

    def test_container_accepts_both_architectures(self):
        for machine in ("x86_64", "aarch64"):
            with self.subTest(machine=machine):
                self.run_install("docker", machine)

    def test_container_rejects_corrupted_archives(self):
        for machine in ("x86_64", "aarch64"):
            with self.subTest(machine=machine):
                self.run_install("docker", machine, corrupt=True)

    def test_container_rejects_unknown_architecture(self):
        self.run_install("docker", "unsupported")


if __name__ == "__main__":
    unittest.main()
