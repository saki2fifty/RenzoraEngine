"""Keep this divergent fork's automation out of official infrastructure."""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[1]
FORK = "saki2fifty/RenzoraEngine"


class ForkWorkflowPolicy(unittest.TestCase):
    def test_automation_has_no_official_destinations(self):
        paths = list((ROOT / ".github").rglob("*.yml"))
        paths += list((ROOT / "docker").rglob("Dockerfile"))
        forbidden = re.compile(
            r"ghcr\.io/renzora(?:/|\b)|renzora/(?:engine|website)\b"
            r"|WEBSITE_SYNC_TOKEN|gh\s+pr\s+create"
        )
        for path in paths:
            with self.subTest(path=path.relative_to(ROOT)):
                self.assertIsNone(forbidden.search(path.read_text()))

    def test_docs_are_read_only_and_stay_in_repository(self):
        text = (ROOT / ".github/workflows/sync-docs.yml").read_text()
        self.assertIn("contents: read", text)
        self.assertIn("actions/upload-artifact@", text)
        for forbidden in ("repository:", "git push", "secrets.", "contents: write"):
            self.assertNotIn(forbidden, text)

    def test_release_and_container_destinations_are_explicit(self):
        release = (ROOT / ".github/workflows/build-engine.yml").read_text()
        images = (ROOT / ".github/workflows/docker-image.yml").read_text()
        self.assertIn(f"GH_REPO: {FORK}", release)
        for text in (release, images):
            self.assertIn(f"github.repository == '{FORK}'", text)
            self.assertIn("ghcr.io/saki2fifty/renzoraengine", text)

    def test_native_checks_do_not_require_published_engine_images(self):
        for name in ("test.yml", "coverage.yml"):
            text = (ROOT / ".github/workflows" / name).read_text()
            self.assertNotIn("ghcr.io/", text)
            self.assertIn("container: rust:1.95.0-bookworm", text)
            self.assertIn("uses: ./.github/actions/native-ci", text)


if __name__ == "__main__":
    unittest.main()
