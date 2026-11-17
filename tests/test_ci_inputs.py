"""固定输入和篡改拒绝契约；pinned-input and tampering-rejection contracts."""

import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("ci_inputs", ROOT / "scripts/ci-inputs.py")
CI_INPUTS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CI_INPUTS)


class CiInputTests(unittest.TestCase):
    """错误固定输入须在执行下载内容之前失败；reject bad inputs before executing downloads."""

    def test_repository_inputs(self):
        """真实工作流也受同一检查约束；check actual workflows with the same validator."""
        CI_INPUTS.check_workflows(ROOT, CI_INPUTS.load_manifest())

    def test_changed_version_requires_matching_tag_and_asset(self):
        """只改版本不能继续使用旧标签或资产；a version-only edit cannot reuse old release inputs."""
        for name in ("nfpm", "appimagetool"):
            manifest = copy.deepcopy(CI_INPUTS.load_manifest())
            manifest["tools"][name]["version"] = "99.0.0"
            with tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "inputs.json"
                path.write_text(json.dumps(manifest), encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "version and release tag"):
                    CI_INPUTS.load_manifest(path)

    def test_mutable_action_and_toolchain_are_rejected(self):
        """可变动作和未登记工具链必须拒绝；reject mutable actions and unrecorded Rust toolchains."""
        samples = (
            "- uses: actions/checkout@v4\n",
            "- uses: dtolnay/rust-toolchain@" + "a" * 40 + "\n",
            "- uses: dtolnay/rust-toolchain@" + "a" * 40 + "\n  with:\n    toolchain: stable\n",
        )
        for sample in samples:
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                workflow = root / ".github/workflows/test.yml"
                workflow.parent.mkdir(parents=True)
                workflow.write_text(sample, encoding="utf-8")
                with self.assertRaises(ValueError):
                    CI_INPUTS.check_workflows(root, CI_INPUTS.load_manifest())

    def test_download_rejects_wrong_digest_before_exposing_tool(self):
        """模拟真实下载出口，错误字节不得成为工具；bad downloaded bytes never become a usable tool."""
        for name in ("nfpm", "appimagetool"):
            manifest = copy.deepcopy(CI_INPUTS.load_manifest())
            asset = manifest["tools"][name]["assets"]["x86_64"]
            with tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)

                def download(*_args, **_kwargs):
                    (directory / asset["name"]).write_bytes(b"tampered tool")

                with patch.object(CI_INPUTS.subprocess, "run", side_effect=download):
                    with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                        CI_INPUTS.download_tool(manifest, name, "x86_64", directory)
                self.assertFalse((directory / manifest["tools"][name]["output"]).exists())

    def test_download_accepts_pinned_digest(self):
        """正确摘要才生成规范工具名；a matching digest exposes the normalized filename."""
        manifest = copy.deepcopy(CI_INPUTS.load_manifest())
        content = b"pinned tool"
        asset = manifest["tools"]["nfpm"]["assets"]["x86_64"]
        asset["sha256"] = hashlib.sha256(content).hexdigest()
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)

            def download(*_args, **_kwargs):
                (directory / asset["name"]).write_bytes(content)

            with patch.object(CI_INPUTS.subprocess, "run", side_effect=download):
                CI_INPUTS.download_tool(manifest, "nfpm", "x86_64", directory)
            self.assertEqual((directory / "nfpm.tar.gz").read_bytes(), content)


if __name__ == "__main__":
    unittest.main()
