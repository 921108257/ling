"""CI 验证器安装的归档安全契约；Archive-safety contracts for the CI verifier installation."""

import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("install_gh", ROOT / "scripts/install-gh.py")
INSTALL_GH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INSTALL_GH)


class GhInstallerTests(unittest.TestCase):
    """错误摘要、链接和路径不能成为可执行文件；Bad digests, links and paths never become executables."""

    def test_supported_platforms(self):
        """五个平台均对应完整摘要；All five supported platforms have complete digest pins."""
        for system, machine in INSTALL_GH.ASSETS:
            filename, digest, member, binary = INSTALL_GH.platform_asset(system, machine)
            self.assertEqual(len(digest), 64)
            self.assertIn(INSTALL_GH.VERSION, filename)
            self.assertTrue(member.endswith("bin/" + binary))
        self.assertEqual(INSTALL_GH.platform_asset("Windows", "AMD64"), INSTALL_GH.platform_asset("Windows", "x86_64"))
        self.assertEqual(INSTALL_GH.platform_asset("Darwin", "arm64"), INSTALL_GH.platform_asset("Darwin", "aarch64"))
        with self.assertRaises(ValueError):
            INSTALL_GH.platform_asset("Windows", "arm64")

    def test_tar_reads_only_fixed_regular_member(self):
        """不调用整包解压，也不写其他成员；Never extract the whole archive or write unrelated members."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "gh.tar.gz"
            with tarfile.open(archive, "w:gz") as output:
                for name in ("../outside", "gh/bin/gh"):
                    member = tarfile.TarInfo(name)
                    member.size = 2
                    output.addfile(member, io.BytesIO(b"gh"))
            INSTALL_GH.extract_binary(archive, "gh/bin/gh", root / "gh")
            self.assertEqual((root / "gh").read_bytes(), b"gh")
            self.assertEqual({path.name for path in root.iterdir()}, {"gh.tar.gz", "gh"})

    def test_tar_link_and_duplicate_rejected(self):
        """固定名称也必须唯一且为普通文件；The fixed name must also be unique and regular."""
        for duplicate in (False, True):
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / "gh.tar.gz"
                with tarfile.open(archive, "w:gz") as output:
                    member = tarfile.TarInfo("gh/bin/gh")
                    member.type = tarfile.SYMTYPE
                    member.linkname = "../../outside"
                    output.addfile(member)
                    if duplicate:
                        output.addfile(member)
                with self.assertRaises(ValueError):
                    INSTALL_GH.extract_binary(archive, "gh/bin/gh", root / "gh")
                self.assertFalse((root / "gh").exists())

    def test_zip_link_rejected(self):
        """ZIP 的 Unix 链接位同样拒绝；Reject Unix symlink attributes in ZIP members too."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "gh.zip"
            with zipfile.ZipFile(archive, "w") as output:
                member = zipfile.ZipInfo("gh/bin/gh")
                member.create_system = 3
                member.external_attr = 0o120777 << 16
                output.writestr(member, "../../outside")
            with self.assertRaises(ValueError):
                INSTALL_GH.extract_binary(archive, "gh/bin/gh", root / "gh")

    def test_bad_download_does_not_change_installed_binary_or_path(self):
        """摘要失败时不覆盖已有 gh 或追加 PATH；A digest failure preserves an existing gh and PATH output."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / f"ling-gh-{INSTALL_GH.VERSION}" / "bin/gh.exe"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"old")
            path = root / "path"
            path.write_text("old-path\n", encoding="utf-8")
            with patch.object(INSTALL_GH, "download", side_effect=ValueError("gh archive SHA256 mismatch")):
                with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                    INSTALL_GH.install(root, path, "Windows", "AMD64")
            self.assertEqual(binary.read_bytes(), b"old")
            self.assertEqual(path.read_text(encoding="utf-8"), "old-path\n")

    def test_download_checks_received_bytes(self):
        """摘要由实际收到的字节决定；The digest is checked against the bytes actually received."""
        class Opener:
            def open(self, *_args, **_kwargs):
                return io.BytesIO(b"altered archive")

        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(INSTALL_GH.urllib.request, "build_opener", return_value=Opener()):
                with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                    INSTALL_GH.download("https://github.com/archive.zip", Path(temporary) / "archive.zip", "0" * 64)


if __name__ == "__main__":
    unittest.main()
