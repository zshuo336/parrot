"""Exercise the actual deployment packer using isolated JARs and build contexts."""
import concurrent.futures
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[2]


class JvmImageArtifacts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="parrot-jvm-image-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        images = self.root / "deploy/images"
        images.mkdir(parents=True)
        original = Path(os.environ.get("PARROT_JVM_PREP_SCRIPT", ROOT / "deploy/images/prep-jvm-libs.sh"))
        shutil.copyfile(original, images / "prep-jvm-libs.sh")
        helper = Path(os.environ.get("PARROT_JVM_PREP_HELPER", ROOT / "deploy/images/prepare_jvm_libs.py"))
        if helper.exists():
            shutil.copyfile(helper, images / "prepare_jvm_libs.py")
        self.cp = self.root / "interop/jvm/target/cp.txt"
        self.cp.parent.mkdir(parents=True)
        self.output = self.root / "gw-jvm-libs.tar"

    def jar(self, name, payload=b"current"):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(path, "w") as archive:
            item = zipfile.ZipInfo("resource.txt", (2020, 1, 1, 0, 0, 0))
            archive.writestr(item, payload)
        return path

    def classpath(self, paths, newline=False):
        self.cp.write_text(os.pathsep.join(map(str, paths)) + ("\n" if newline else ""))

    def run_packer(self):
        return subprocess.run(["bash", str(self.root / "deploy/images/prep-jvm-libs.sh")],
                              cwd=self.tmp.name, capture_output=True, text=True, timeout=30)

    def packed(self):
        with tarfile.open(self.output) as archive:
            members = [m for m in archive.getmembers() if m.isfile()]
            self.assertEqual(len({Path(m.name).name for m in members}), len(members))
            return {Path(m.name).name: archive.extractfile(m).read() for m in members}

    def success(self):
        result = self.run_packer()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_single_dependency_without_trailing_newline_is_packaged(self):
        jar = self.jar("repository/only.jar")
        self.classpath([jar])
        self.success()
        self.assertEqual(self.packed(), {jar.name: jar.read_bytes()})

    def test_changed_dependency_with_unchanged_classpath_and_mtime_is_rebuilt(self):
        jar = self.jar("repository/a.jar", b"old")
        self.classpath([jar], newline=True)
        self.success()
        stamp = jar.stat()
        self.jar("repository/a.jar", b"new")
        os.utime(jar, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        self.success()
        self.assertEqual(self.packed()[jar.name], jar.read_bytes())

    def test_corrupt_existing_archive_is_not_a_cache_hit(self):
        jar = self.jar("repository/a.jar")
        self.classpath([jar], newline=True)
        self.output.write_bytes(b"broken archive")
        self.success()
        self.assertEqual(self.packed()[jar.name], jar.read_bytes())

    def test_conflicting_basenames_fail_without_replacing_prior_archive(self):
        first = self.jar("group-a/library.jar", b"first")
        second = self.jar("group-b/library.jar", b"second")
        self.output.write_bytes(b"previous artifact")
        self.classpath([first, second], newline=True)
        os.utime(self.output, (1, 1))
        result = self.run_packer()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.output.read_bytes(), b"previous artifact")

    def test_missing_last_dependency_is_rejected_without_publishing_partial_archive(self):
        jar = self.jar("repository/a.jar")
        self.classpath([jar, self.root / "missing.jar"])
        self.output.write_bytes(b"previous artifact")
        os.utime(self.output, (1, 1))
        self.assertNotEqual(self.run_packer().returncode, 0)
        self.assertEqual(self.output.read_bytes(), b"previous artifact")

    def test_empty_classpath_is_rejected(self):
        self.cp.write_text("")
        self.assertNotEqual(self.run_packer().returncode, 0)
        self.assertFalse(self.output.exists())

    def test_empty_classpath_element_is_rejected(self):
        jar = self.jar("repository/a.jar")
        self.cp.write_text(str(jar) + os.pathsep)
        self.assertNotEqual(self.run_packer().returncode, 0)
        self.assertFalse(self.output.exists())

    def test_dependency_paths_with_spaces_and_unicode_are_preserved(self):
        jars = [self.jar("cache with spaces/alpha.jar"), self.jar("cache/依赖.jar")]
        self.classpath(jars, newline=True)
        self.success()
        self.assertEqual(self.packed(), {p.name: p.read_bytes() for p in jars})

    def test_duplicate_identical_path_has_one_archive_member(self):
        jar = self.jar("cache/a.jar")
        self.classpath([jar, jar], newline=True)
        self.success()
        self.assertEqual(self.packed(), {jar.name: jar.read_bytes()})

    def test_symlink_keeps_the_classpath_basename(self):
        jar = self.jar("cache/original.jar")
        alias = self.root / "alias.jar"
        alias.symlink_to(jar)
        self.classpath([alias])
        self.success()
        self.assertEqual(self.packed(), {alias.name: jar.read_bytes()})

    def test_invalid_jar_is_rejected(self):
        path = self.root / "bad.jar"
        path.write_bytes(b"not a zip archive")
        self.classpath([path], newline=True)
        self.assertNotEqual(self.run_packer().returncode, 0)
        self.assertFalse(self.output.exists())

    def test_same_bytes_produce_identical_archive_despite_source_timestamps(self):
        jar = self.jar("cache/a.jar")
        self.classpath([jar], newline=True)
        self.success()
        original = self.output.read_bytes()
        self.output.unlink()
        os.utime(jar, (2000000000, 2000000000))
        self.success()
        self.assertEqual(self.output.read_bytes(), original)

    def test_concurrent_packers_publish_complete_archives_without_shared_temp_files(self):
        jars = [self.jar(f"cache/a{i}.jar", bytes([i]) * 4096) for i in range(16)]
        self.classpath(jars, newline=True)
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as workers:
            results = list(workers.map(lambda _: self.run_packer(), range(8)))
        for result in results:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.packed(), {p.name: p.read_bytes() for p in jars})
        self.assertFalse(list(self.root.glob("gw-jvm-libs.tar.*")), "temporary archives leaked")


if __name__ == "__main__":
    unittest.main()
