"""Image selection and ELF contracts; fixtures do not prove Linux execution."""
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
VALIDATOR = Path(os.environ.get("PARROT_APP_VALIDATOR", ROOT / "deploy/images/validate_app_binary.py"))


def executable(machine=183, kind=2, interpreter=None):
    """Minimal executable mappings, with independent x86/ARM exit(0) instructions."""
    code = bytes.fromhex("b83c00000031ff0f05") if machine == 62 else bytes.fromhex("000080d2a80b80d2010000d4")
    phnum = 1 if interpreter is None else 2
    offset = 64 + phnum * 56
    data = bytearray(b"\x7fELF\x02\x01\x01" + b"\0" * 9)
    data += struct.pack("<HHIQQQIHHHHHH", kind, machine, 1, 0x400000 + offset, 64, 0, 0, 64, 56, phnum, 0, 0, 0)
    length = offset + len(code) + (len(interpreter) + 1 if interpreter else 0)
    data += struct.pack("<IIQQQQQQ", 1, 5, 0, 0x400000, 0, length, length, 4096)
    if interpreter:
        data += struct.pack("<IIQQQQQQ", 3, 4, offset + len(code), 0, 0, len(interpreter) + 1, len(interpreter) + 1, 1)
    data += code
    if interpreter:
        data += interpreter.encode() + b"\0"
    return data


class AppImageArtifactsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.target = self.root / "target"
        self.output = self.root / "validated/app"

    def put(self, data, triple="aarch64-unknown-linux-musl", binary="websearch"):
        path = self.target / triple / "release" / binary
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def run_validator(self, *args, success=True):
        result = subprocess.run([sys.executable, str(VALIDATOR), "--target-root", str(self.target),
                                 "--output", str(self.output), "--target-os", "linux", "--arch", "arm64", *args],
                                capture_output=True, text=True, timeout=10)
        if success:
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertFalse(self.output.exists(), "rejected artifact was published")
        return result

    def test_auto_selects_target_not_host_or_other_present_binary(self):
        arm = executable()
        amd = executable(62)
        self.put(arm)
        self.put(amd, "x86_64-unknown-linux-musl")
        self.put(b"native Mach-O", "")
        for arch, expected in [("arm64", arm), ("amd64", amd)]:
            self.run_validator("--arch", arch)
            self.assertEqual(self.output.read_bytes(), expected)
            self.assertEqual(self.output.stat().st_mode & 0o777, 0o755)

    def test_mislabeled_binary_is_rejected(self):
        self.put(executable(62))
        result = self.run_validator(success=False)
        self.assertIn("does not match", result.stderr)

    def test_explicit_wrong_architecture_path_is_rejected(self):
        self.put(executable(62), "x86_64-unknown-linux-musl")
        self.run_validator("--triple", "x86_64-unknown-linux-musl", success=False)

    def test_missing_target_never_falls_back_to_native(self):
        self.put(executable(), "")
        self.run_validator(success=False)

    def test_explicit_native_and_custom_paths_remain_supported(self):
        for triple in ["", "aarch64-unknown-linux-gnu"]:
            with self.subTest(triple=triple):
                self.put(executable(kind=3), triple, "custom-tool")
                self.run_validator("--triple", triple, "--bin", "custom-tool")
                self.assertEqual(self.output.read_bytes(), executable(kind=3))

    def test_dynamic_executable_is_not_mistaken_for_shared_library(self):
        data = executable(kind=3, interpreter="/lib/ld-linux-aarch64.so.1")
        self.put(data, "")
        self.run_validator("--triple", "")
        self.assertEqual(self.output.read_bytes(), data)

    def test_unsupported_platforms_are_rejected(self):
        self.put(executable())
        for args in [("--arch", "386"), ("--target-os", "darwin")]:
            with self.subTest(args=args):
                self.run_validator(*args, success=False)

    def test_path_traversal_is_rejected(self):
        self.put(executable())
        for option, value in [("--bin", "../websearch"), ("--bin", "/tmp/app"),
                              ("--bin", ""), ("--triple", "../target"), ("--triple", "/")]:
            with self.subTest(option=option, value=value):
                self.run_validator(option, value, success=False)

    def test_non_elf_and_incompatible_encodings_are_rejected(self):
        for data in [b"\xcf\xfa\xed\xfe" + b"\0" * 100, b"MZ" + b"\0" * 100,
                     executable()[:4] + b"\x01" + executable()[5:],
                     executable()[:5] + b"\x02" + executable()[6:], b"\x7fELF"]:
            with self.subTest(prefix=data[:7]):
                self.put(data)
                self.run_validator(success=False)

    def test_truncated_program_headers_and_segments_are_rejected(self):
        for end in [70, 119, len(executable()) - 1]:
            with self.subTest(end=end):
                self.put(executable()[:end])
                self.run_validator(success=False)

    def test_shared_library_or_non_executable_entry_is_rejected(self):
        for entry, flags in [(0, 5), (0x800000, 5), (0x400078, 4)]:
            with self.subTest(entry=entry, flags=flags):
                data = executable(kind=3)
                struct.pack_into("<Q", data, 24, entry)
                struct.pack_into("<I", data, 68, flags)
                self.put(data)
                self.run_validator(success=False)

    def test_invalid_header_and_load_size_are_rejected(self):
        for offset, fmt, value in [(16, "H", 1), (20, "I", 2), (52, "H", 1),
                                   (54, "H", 55), (56, "H", 0), (32, "Q", 2), (104, "Q", 1)]:
            with self.subTest(offset=offset):
                data = executable()
                struct.pack_into("<" + fmt, data, offset, value)
                self.put(data)
                self.run_validator(success=False)


if __name__ == "__main__":
    unittest.main()
