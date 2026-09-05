#!/usr/bin/env python3
"""Pack release executables while preserving opaque runtime deployment records.

The Rust contract owns the record format. This packaging adapter copies its
bytes unchanged; it never invents a feature list. Already packed container
artifacts are inspected on a temporary copy, never executed.
"""

import pathlib
import shutil
import subprocess
import sys
import tempfile

PREFIX = b"\0RenzoraRuntimeBuiltinPolicy\0"
SUFFIX = b"\0EndRenzoraRuntimeBuiltinPolicy\0"
LENGTH = len(PREFIX) + 3 + len(SUFFIX)


def record(path):
    found = None
    carry = b""
    with path.open("rb") as source:
        while chunk := source.read(65536):
            data = carry + chunk
            start = 0
            while (start := data.find(PREFIX, start)) >= 0:
                candidate = data[start:start + LENGTH]
                if (len(candidate) == LENGTH and candidate.endswith(SUFFIX)
                        and candidate[len(PREFIX)] == 1
                        and candidate[len(PREFIX) + 1] ^ candidate[len(PREFIX) + 2] == 255):
                    if found is not None and found != candidate:
                        raise ValueError("conflicting runtime capability records")
                    found = candidate
                start += 1
            carry = data[-(LENGTH - 1):]
    return found


def compress(upx, path):
    original = record(path)
    packed = subprocess.run([upx, "-t", str(path)], capture_output=True).returncode == 0
    # Older UPX versions cannot recognize an ELF with trailing metadata.
    # Test a copy without our exact footer instead of double-packing the input.
    if not packed and original is not None and path.stat().st_size >= LENGTH:
        with path.open("rb") as source:
            source.seek(-LENGTH, 2)
            trailing = source.read() == original
        if trailing:
            with tempfile.TemporaryDirectory(prefix="renzora-upx-test-") as directory:
                copy = pathlib.Path(directory) / path.name
                shutil.copy2(path, copy)
                with copy.open("r+b") as output:
                    output.truncate(copy.stat().st_size - LENGTH)
                packed = subprocess.run([upx, "-t", str(copy)], capture_output=True).returncode == 0
    if packed and original is not None:
        return
    if packed:
        with tempfile.TemporaryDirectory(prefix="renzora-upx-") as directory:
            copy = pathlib.Path(directory) / path.name
            shutil.copy2(path, copy)
            subprocess.run([upx, "-d", "-q", str(copy)], check=True)
            original = record(copy)
    else:
        result = subprocess.run([upx, "--best", "--lzma", "-q", str(path)])
        if result.returncode != 0:
            print(f"WARN: UPX declined {path}; keeping the uncompressed input")
            return
    if original is not None:
        with path.open("ab") as output:
            output.write(original)


if __name__ == "__main__":
    compress(sys.argv[1], pathlib.Path(sys.argv[2]))
