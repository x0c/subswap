"""Run the real PowerShell installer against isolated release/download fixtures."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile


INSTALLER = Path(__file__).resolve().parents[1] / "install.ps1"


class InstallerTests(unittest.TestCase):
    def run_case(self, *, draft=False, release_id=False, mismatch=False,
                 corrupt=False, authenticated=True):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            archive = root / "payload.zip"
            with zipfile.ZipFile(archive, "w") as payload:
                payload.writestr("subswap/subswap.exe", b"new executable fixture")
            checksum = "0" * 64 if corrupt else hashlib.sha256(archive.read_bytes()).hexdigest()
            (root / "payload.sha256").write_text(checksum, encoding="utf-8")
            tag = "v9.9.8" if mismatch else "v9.9.9"
            name = f"subswap-{tag}-x86_64-pc-windows-msvc.zip"
            release = {"tag_name": tag, "draft": draft, "assets": [
                {"name": name, "url": "https://api.github.com/fixture/archive",
                 "browser_download_url": "https://github.com/fixture/archive"},
                {"name": name + ".sha256", "url": "https://api.github.com/fixture/checksum",
                 "browser_download_url": "https://github.com/fixture/checksum"},
            ]}
            (root / "release.json").write_text(json.dumps(release), encoding="utf-8")
            destination = root / "installed"
            destination.mkdir()
            executable = destination / "subswap.exe"
            executable.write_bytes(b"previous working installation")
            script = root / "fixture.ps1"
            script.write_text(r'''
function Invoke-RestMethod {
    param($Uri, $Headers)
    $expected = if ($env:TEST_RELEASE_ID -eq "1") { "/releases/123" } else { "/releases/tags/v9.9.9" }
    if (-not $Uri.EndsWith($expected)) { throw "Wrong release lookup: $Uri" }
    Get-Content (Join-Path $env:TEST_ROOT "release.json") -Raw | ConvertFrom-Json
}
function Invoke-WebRequest {
    param($Uri, $Headers, $OutFile, [switch]$UseBasicParsing)
    if ($env:TEST_RELEASE_ID -eq "1") {
        if (-not $Uri.StartsWith("https://api.github.com/fixture/")) { throw "Draft used public URL" }
        if ($Headers.Accept -ne "application/octet-stream") { throw "Missing binary Accept" }
        if (-not $Headers.Authorization) { throw "Missing authentication" }
    } elseif (-not $Uri.StartsWith("https://github.com/fixture/")) { throw "Wrong public URL" }
    $source = if ($Uri.EndsWith("checksum")) { "payload.sha256" } else { "payload.zip" }
    Copy-Item (Join-Path $env:TEST_ROOT $source) $OutFile
}
$arguments = @{Version = "9.9.9"; SkipPathUpdate = $true}
if ($env:TEST_RELEASE_ID -eq "1") { $arguments.ReleaseId = 123 }
& $env:TEST_INSTALLER @arguments
''', encoding="utf-8")
            env = dict(os.environ, TEST_ROOT=str(root), TEST_INSTALLER=str(INSTALLER),
                       TEST_RELEASE_ID="1" if release_id else "0",
                       SUBSWAP_INSTALL_DIR=str(destination), SUBSWAP_REPOSITORY="example/fixture")
            env.pop("GH_TOKEN", None)
            env.pop("GITHUB_TOKEN", None)
            if authenticated:
                env["GH_TOKEN"] = "synthetic-installer-test-token"
            result = subprocess.run([os.environ.get("PWSH", shutil.which("pwsh") or "pwsh"),
                                     "-NoProfile", "-NonInteractive", "-File", str(script)],
                                    env=env, capture_output=True, text=True, timeout=30)
            return result, executable.read_bytes()

    def test_public_release(self):
        result, data = self.run_case(authenticated=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(data, b"new executable fixture")

    def test_authenticated_draft_by_id(self):
        result, data = self.run_case(draft=True, release_id=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(data, b"new executable fixture")

    def test_rejections_preserve_installed_binary(self):
        for options, message in [
            ({"draft": True}, "still a draft"),
            ({"release_id": True, "mismatch": True}, "does not match"),
            ({"release_id": True, "authenticated": False}, "requires"),
            ({"corrupt": True}, "SHA256 verification failed"),
        ]:
            with self.subTest(options=options):
                result, data = self.run_case(**options)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertEqual(data, b"previous working installation")


if __name__ == "__main__":
    unittest.main()
