"""Release adapter tests; the native acceptance run also checks real Rust bytes."""

import importlib.util
import pathlib
import tempfile
import types
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "compress_runtime", pathlib.Path(__file__).with_name("compress-runtime.py")
)
ADAPTER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ADAPTER)


class Records(unittest.TestCase):
    def test_older_upx_detection_does_not_double_pack_or_modify_input(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "runtime"
            record = ADAPTER.PREFIX + bytes([1, 255, 0]) + ADAPTER.SUFFIX
            original = b"packed payload" + record
            path.write_bytes(original)

            def probe(arguments, **_kwargs):
                self.assertEqual(arguments[1], "-t")
                data = pathlib.Path(arguments[2]).read_bytes()
                return types.SimpleNamespace(returncode=0 if data == b"packed payload" else 1)

            with mock.patch.object(ADAPTER.subprocess, "run", side_effect=probe) as run:
                ADAPTER.compress("upx", path)
            self.assertEqual(run.call_count, 2)
            self.assertEqual(path.read_bytes(), original)

    def test_masks_boundaries_and_conflicts(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "runtime"
            for mask in range(256):
                record = ADAPTER.PREFIX + bytes([1, mask, mask ^ 255]) + ADAPTER.SUFFIX
                path.write_bytes(b"x" * 65530 + record + record)
                self.assertEqual(ADAPTER.record(path), record)
            other = ADAPTER.PREFIX + bytes([1, 0, 255]) + ADAPTER.SUFFIX
            path.write_bytes(record + other)
            with self.assertRaisesRegex(ValueError, "conflicting"):
                ADAPTER.record(path)

    def test_no_record_and_corruption(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "runtime"
            record = ADAPTER.PREFIX + bytes([1, 0, 255]) + ADAPTER.SUFFIX
            for length in range(len(record)):
                path.write_bytes(record[:length])
                self.assertIsNone(ADAPTER.record(path))
            for index in range(len(record)):
                corrupt = bytearray(record)
                corrupt[index] ^= 1
                path.write_bytes(corrupt)
                self.assertIsNone(ADAPTER.record(path))


if __name__ == "__main__":
    unittest.main()
