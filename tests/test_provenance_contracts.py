"""来源校验的选择和失败顺序；Selection and fail-closed ordering for provenance checks."""

import argparse
from contextlib import redirect_stdout
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("provenance_contracts", ROOT / "tests/provenance_contracts.py")
PROVENANCE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROVENANCE)
COMMIT = "4" * 40
REF = "refs/tags/v0.2.0"


class ProvenanceTests(unittest.TestCase):
    """仅模拟验证器边界，不宣称执行密码学验签；Mocks test policy wiring, not real cryptographic verification."""

    def fixture(self, root):
        """两个独立包允许检查只下载所选包；Two subjects exercise selected-asset downloads."""
        first = root / "ling_0.2.0_linux_x86_64.deb"
        second = root / "ling_0.2.0_linux_x86_64.rpm"
        first.write_bytes(b"deb fixture")
        second.write_bytes(b"rpm fixture")
        (root / "SHA256SUMS").write_text(
            "".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in (first, second)),
            encoding="utf-8",
        )
        (root / "PROVENANCE.sigstore.json").write_text("fixture proof", encoding="utf-8")
        return first, second

    def run_main(self, root, *arguments):
        """调用实际 CLI 参数路径；Exercise the real CLI argument path."""
        with patch.object(sys, "argv", ["provenance_contracts.py", "--dist", str(root), "--ref", REF, *arguments]), redirect_stdout(io.StringIO()):
            PROVENANCE.main()

    def test_selection_allows_absent_unselected_packages(self):
        """已签清单可列出未下载包，但所选包必须存在；Only selected packages need to exist locally."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first, second = self.fixture(root)
            second.unlink()
            self.assertEqual(list(PROVENANCE.checksums(root / "SHA256SUMS", [first.name])), [first.name])
            with self.assertRaisesRegex(ValueError, "missing from disk"):
                PROVENANCE.checksums(root / "SHA256SUMS")

    def test_rejects_duplicate_unsafe_and_unknown_names(self):
        """跨平台路径、重复及未列出名称均拒绝；Reject cross-platform paths, duplicates and unknown subjects."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first, _ = self.fixture(root)
            sums = root / "SHA256SUMS"
            valid = sums.read_text(encoding="utf-8")
            for name in ("../outside", "dir/file", "dir\\file", "C:outside", "bad\tname"):
                with self.subTest(name=name):
                    sums.write_text(f"{'0' * 64}  {name}\n", encoding="utf-8")
                    with self.assertRaisesRegex(ValueError, "Invalid checksum"):
                        PROVENANCE.checksums(sums)
            sums.write_text(valid + valid.splitlines()[0] + "\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Duplicate checksum"):
                PROVENANCE.checksums(sums)
            sums.write_text(valid, encoding="utf-8")
            for selected in ([first.name, first.name], ["missing.deb"], []):
                with self.subTest(selected=selected), self.assertRaises(ValueError):
                    PROVENANCE.checksums(sums, selected)

    def test_rejects_incomplete_source_commit(self):
        """来源约束不接受缩写或 shell 文本；Source constraints reject abbreviated commits and shell text."""
        self.assertEqual(PROVENANCE.source_digest(COMMIT), COMMIT)
        for value in ("main", "a" * 39, "A" * 40, "a" * 40 + "\n"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                PROVENANCE.source_digest(value)

    def test_manifest_is_verified_before_it_is_parsed(self):
        """拒绝证明时不能相信甚至解析摘要清单；A rejected proof must stop before parsing the manifest."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            (root / "SHA256SUMS").write_text("invalid manifest", encoding="utf-8")
            with patch.object(PROVENANCE.subprocess, "run", return_value=subprocess.CompletedProcess([], 1, "", "rejected")) as run:
                with self.assertRaisesRegex(AssertionError, "rejected"):
                    self.run_main(root)
            self.assertEqual(len(run.call_args_list), 1)
            self.assertEqual(Path(run.call_args.args[0][3]).name, "SHA256SUMS")

    def test_invalid_success_output_and_mismatched_subject_are_rejected(self):
        """退出零也必须返回匹配的验证主体；Exit zero still requires a matching verified subject."""
        for output in ("", "[]", "{}", json.dumps([{"verificationResult": {"statement": {"subject": [{"name": "wrong", "digest": {"sha256": "0" * 64}}]}}}])):
            with self.subTest(output=output), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                self.fixture(root)
                with patch.object(PROVENANCE.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, output, "")):
                    with self.assertRaises((AssertionError, json.JSONDecodeError, TypeError)):
                        self.run_main(root)

    def test_selected_subject_policy_and_adversarial_calls(self):
        """每次调用绑定完整身份，坏证明不得变为成功；Every verification binds the full identity policy."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first, second = self.fixture(root)
            second.unlink()

            def verify(command, **kwargs):
                path = Path(command[3])
                proof = Path(command[command.index("--bundle") + 1])
                valid = (path.parent == root and proof == root / "PROVENANCE.sigstore.json"
                         and command[command.index("--repo") + 1] == "921108257/ling"
                         and command[command.index("--source-ref") + 1] == REF
                         and command[command.index("--source-digest") + 1] == COMMIT)
                statement = [{"verificationResult": {"statement": {"subject": [{
                    "name": path.name, "digest": {"sha256": hashlib.sha256(path.read_bytes()).hexdigest()},
                }]}}}]
                self.assertEqual(kwargs["env"]["HTTPS_PROXY"], "http://127.0.0.1:9")
                return subprocess.CompletedProcess(command, 0 if valid else 1, json.dumps(statement), "rejected")

            with patch.object(PROVENANCE.subprocess, "run", side_effect=verify) as run:
                self.run_main(root, "--asset", first.name, "--source-digest", COMMIT)
            self.assertEqual(len(run.call_args_list), 9)
            self.assertEqual(Path(run.call_args_list[0].args[0][3]).name, "SHA256SUMS")
            for call in run.call_args_list:
                command = call.args[0]
                self.assertIn("--deny-self-hosted-runners", command)
                for flag, expected in (
                    ("--custom-trusted-root", str(ROOT / "crates/server/src/update/trusted-root.json")),
                    ("--cert-oidc-issuer", "https://token.actions.githubusercontent.com"),
                    ("--predicate-type", "https://slsa.dev/provenance/v1"),
                ):
                    self.assertEqual(command[command.index(flag) + 1], expected)
                reference = command[command.index("--source-ref") + 1]
                self.assertEqual(command[command.index("--cert-identity") + 1],
                                 f"https://github.com/921108257/ling/.github/workflows/release.yml@{reference}")

    def test_tampered_asset_fails_before_its_provenance_call(self):
        """清单通过后摘要错误仍立即拒绝；A verified manifest does not permit altered asset bytes."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first, _ = self.fixture(root)
            first.write_bytes(b"changed bytes")
            manifest = root / "SHA256SUMS"
            output = json.dumps([{"verificationResult": {"statement": {"subject": [{
                "name": manifest.name, "digest": {"sha256": hashlib.sha256(manifest.read_bytes()).hexdigest()},
            }]}}}])
            with patch.object(PROVENANCE.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, output, "")) as run:
                with self.assertRaises(AssertionError):
                    self.run_main(root, "--asset", first.name)
            self.assertEqual(len(run.call_args_list), 1)


if __name__ == "__main__":
    unittest.main()
