"""独立构建测量辅助函数契约；Contracts for independent-build measurement helpers."""

import importlib.util
from pathlib import Path
import unittest


SPEC = importlib.util.spec_from_file_location("reproducible_build", Path(__file__).resolve().parents[1] / "scripts/reproducible-build.py")
REPRO = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REPRO)


class ReproducibleBuildTests(unittest.TestCase):
    """保留真实差异并消除路径因素；Preserve real differences while normalizing build paths."""

    def test_existing_flags_and_independent_target_paths(self):
        """有空格的路径在编码标志中保持一个参数；Paths containing spaces remain one encoded flag."""
        environment = {"CARGO_ENCODED_RUSTFLAGS": "-C\x1ftarget-feature=+crt-static", "RUSTFLAGS": "ignored"}
        flags = REPRO.build_flags(environment, Path("source root"), Path("build a"), "/ling")
        self.assertEqual(flags[:2], ["-C", "target-feature=+crt-static"])
        self.assertIn("--remap-path-prefix=source root=/ling/src", flags)
        self.assertIn("--remap-path-prefix=build a=/ling/target", flags)

    def test_comparison_reports_differences_without_error(self):
        """大小相同不代表二进制相同；Equal file sizes do not imply equal binary contents."""
        first = {"ling": {"sha256": "abc", "bytes": 123}}
        second = {"ling": {"sha256": "def", "bytes": 123}}
        self.assertEqual(REPRO.compare(first, second), {"ling": {"same_sha256": False, "same_bytes": True}})
        self.assertTrue(REPRO.compare(first, first)["ling"]["same_sha256"])


if __name__ == "__main__":
    unittest.main()
