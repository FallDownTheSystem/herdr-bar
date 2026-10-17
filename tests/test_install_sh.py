"""scripts/install.sh's download path, against a file:// release directory and
a fake binary (a shell script), so no cargo or network is needed."""

import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SH = shutil.which("sh")
CURL = shutil.which("curl")


def manifest_version():
    with open(os.path.join(ROOT, "herdr-plugin.toml"), encoding="utf-8") as handle:
        for line in handle:
            if line.startswith("version"):
                return line.split('"')[1]
    raise AssertionError("no version in herdr-plugin.toml")


def asset_name():
    # The same names install.sh computes; run its own function so the two
    # cannot drift apart.
    script = os.path.join(ROOT, "scripts", "install.sh")
    with open(script, encoding="utf-8") as handle:
        text = handle.read()
    start = text.index("asset_name() {")
    end = text.index("\n}\n", start) + 3
    result = subprocess.run(
        [SH, "-c", text[start:end] + "asset_name"],
        stdout=subprocess.PIPE,
        universal_newlines=True,
    )
    return result.stdout.strip() if result.returncode == 0 else None


@unittest.skipIf(sys.platform == "win32", "install.sh is the Unix build step")
@unittest.skipIf(SH is None or CURL is None, "needs sh and curl")
class InstallShTest(unittest.TestCase):
    def setUp(self):
        self.asset = asset_name()
        if not self.asset:
            self.skipTest("no prebuilt asset for this machine")
        self.version = manifest_version()
        self.tmp = tempfile.mkdtemp(prefix="herdr-bar-install-")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.plugin = os.path.join(self.tmp, "plugin")
        os.makedirs(os.path.join(self.plugin, "scripts"))
        scripts = os.path.join(self.plugin, "scripts")
        shutil.copy(os.path.join(ROOT, "scripts", "install.sh"), scripts)
        shutil.copy(os.path.join(ROOT, "herdr-plugin.toml"), self.plugin)
        self.release = os.path.join(self.tmp, "release")
        os.makedirs(self.release)
        # A PATH with the tools install.sh uses but no cargo, so a failed
        # download fails the install instead of building from source.
        self.path_dir = os.path.join(self.tmp, "path")
        os.makedirs(self.path_dir)
        for tool in ("curl", "sha256sum", "shasum", "awk", "sed", "head", "uname", "sysctl",
                     "mktemp", "mkdir", "cp", "mv", "rm", "chmod", "dirname", "perl"):
            found = shutil.which(tool)
            if found:
                os.symlink(found, os.path.join(self.path_dir, tool))

    def publish(self, reported, checksum=None, asset=True):
        body = "#!/bin/sh\necho '%s'\n" % reported
        if asset:
            with open(os.path.join(self.release, self.asset), "w") as handle:
                handle.write(body)
        digest = checksum or hashlib.sha256(body.encode()).hexdigest()
        with open(os.path.join(self.release, "SHA256SUMS"), "w") as handle:
            handle.write("%s  %s\n" % (digest, self.asset))

    def run_install(self, base=None):
        env = {
            "PATH": self.path_dir,
            "HOME": self.tmp,
            "TMPDIR": self.tmp,
            "HERDR_BAR_DOWNLOAD_BASE": base or "file://" + self.release,
        }
        return subprocess.run(
            [SH, os.path.join(self.plugin, "scripts", "install.sh")],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            timeout=60,
        )

    def target(self):
        return os.path.join(self.plugin, "bin", "herdr-bar")

    def assert_clean_bin(self):
        bin_dir = os.path.join(self.plugin, "bin")
        names = os.listdir(bin_dir) if os.path.isdir(bin_dir) else []
        leftovers = [name for name in names if name != "herdr-bar"]
        self.assertEqual(leftovers, [])

    def test_good_asset_installs(self):
        self.publish("herdr-bar %s" % self.version)
        result = self.run_install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("installed", result.stderr)
        self.assertTrue(os.access(self.target(), os.X_OK))
        self.assert_clean_bin()

    def test_checksum_mismatch_fails(self):
        self.publish("herdr-bar %s" % self.version, checksum="0" * 64)
        result = self.run_install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse(os.path.exists(self.target()))
        self.assert_clean_bin()

    def test_missing_asset_fails(self):
        self.publish("herdr-bar %s" % self.version, asset=False)
        result = self.run_install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("download failed", result.stderr)
        self.assertIn("herdr plugin install FallDownTheSystem/herdr-bar --ref windows-support", result.stderr)
        self.assertFalse(os.path.exists(self.target()))

    def test_wrong_version_fails(self):
        self.publish("herdr-bar 0.0.0")
        result = self.run_install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("reports 'herdr-bar 0.0.0'", result.stderr)
        self.assertFalse(os.path.exists(self.target()))
        self.assert_clean_bin()

    def test_http_base_warns(self):
        # Nothing listens on port 9; only the warning matters.
        result = self.run_install(base="http://127.0.0.1:9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("warning: HERDR_BAR_DOWNLOAD_BASE is not https", result.stderr)


if __name__ == "__main__":
    unittest.main()
