"""固定输入和篡改拒绝契约；pinned-input and tampering-rejection contracts."""

import copy
from contextlib import redirect_stdout
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
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

    def test_release_baseline_identity_and_output(self):
        """下载标签和证明来源共享已固定身份；Downloads and provenance share the pinned identity."""
        with patch.object(sys, "argv", ["ci-inputs.py", "baseline"]), redirect_stdout(io.StringIO()) as output:
            CI_INPUTS.main()
        self.assertEqual(output.getvalue().splitlines(), [
            "version=0.2.0", "ref=refs/tags/v0.2.0", "commit=441ffc7dbb2bf48d6257c0ca3b831f099554af1a",
        ])

    def test_release_baseline_rejects_partial_or_unsafe_identity(self):
        """非完整版本和可变/不完整 commit 无法进入工作流；Reject incomplete or unsafe baseline identities."""
        for key, value in (("version", "v0.2.0"), ("version", "0.2"), ("version", "00.2.0"),
                           ("version", "0.2.0\nextra=1"), ("commit", "main"), ("commit", "a" * 39),
                           ("commit", "A" * 40)):
            with self.subTest(key=key, value=value), tempfile.TemporaryDirectory() as temporary:
                manifest = copy.deepcopy(CI_INPUTS.load_manifest())
                manifest["release_baseline"][key] = value
                path = Path(temporary) / "inputs.json"
                path.write_text(json.dumps(manifest), encoding="utf-8")
                with self.assertRaises(ValueError):
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


class PublishedTargetTests(unittest.TestCase):
    """运行实际解析器，外部进程只模拟；Run the workflow resolver with mocked external processes."""

    def test_target_identity_is_validated_before_outputs(self):
        """输入、公开标签、commit及源码版本都要一致；Inputs, public tag, commit and source version must agree."""
        workflow = (ROOT / ".github/workflows/native-update.yml").read_text(encoding="utf-8")
        source = textwrap.dedent(workflow.split("python3 - <<'PY'\n", 1)[1].split("\n          PY", 1)[0])
        cases = (
            ("0.3.0", "v0.3.0", False, "0.3.0", "a" * 40, True),
            ("0.2.0", "v0.2.0", False, "0.2.0", "a" * 40, False),
            ("0.1.9", "v0.1.9", False, "0.1.9", "a" * 40, False),
            ("0.3.0;echo unsafe", "v0.3.0", False, "0.3.0", "a" * 40, False),
            ("0.3.0\ncommit=unsafe", "v0.3.0", False, "0.3.0", "a" * 40, False),
            ("0.3.0", "v0.3.0", True, "0.3.0", "a" * 40, False),
            ("0.3.0", "v0.3.1", False, "0.3.0", "a" * 40, False),
            ("0.3.0", "v0.3.0", False, "0.3.1", "a" * 40, False),
            ("0.3.0", "v0.3.0", False, "0.3.0", "a" * 39, False),
        )
        for version, tag, draft, source_version, commit, success in cases:
            with self.subTest(version=version, tag=tag, draft=draft, source_version=source_version, commit=commit), tempfile.TemporaryDirectory() as temporary:
                output = Path(temporary) / "outputs"

                def query(command, **_kwargs):
                    if command[:3] == ["gh", "release", "view"]:
                        return json.dumps({"tagName": tag, "isDraft": draft, "url": "fixture"})
                    if command[:2] == ["git", "rev-parse"]:
                        return commit + "\n"
                    if command[:2] == ["git", "show"]:
                        return f'[workspace.package]\nversion = "{source_version}"\n'
                    raise AssertionError(command)

                environment = {"REQUESTED_VERSION": version, "BASELINE_VERSION": "0.2.0", "GITHUB_OUTPUT": str(output)}
                with patch.dict(os.environ, environment), patch.object(subprocess, "check_output", side_effect=query) as check, patch.object(subprocess, "run") as run, redirect_stdout(io.StringIO()):
                    if success:
                        exec(compile(source, "native-update.yml:resolve", "exec"), {})
                    else:
                        with self.assertRaises(ValueError):
                            exec(compile(source, "native-update.yml:resolve", "exec"), {})
                if success:
                    self.assertEqual(output.read_text(encoding="utf-8"), f"version=0.3.0\ncommit={commit}\n")
                    self.assertEqual(run.call_args.args[0], ["git", "fetch", "--no-tags", "origin", "refs/tags/v0.3.0"])
                else:
                    self.assertFalse(output.exists())
                if version != "0.3.0":
                    self.assertFalse(check.called)
                    self.assertFalse(run.called)


if __name__ == "__main__":
    unittest.main()
