"""Cargo 许可到 SPDX 表达式的转换契约；Contracts for Cargo-to-SPDX license expressions."""

import importlib.util
from pathlib import Path
import unittest


SPEC = importlib.util.spec_from_file_location("generate_sbom", Path(__file__).resolve().parents[1] / "packaging/generate-sbom.py")
SBOM = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SBOM)


class SbomLicenseTests(unittest.TestCase):
    """保留许可选择语义，不改已有 SPDX 运算符；Preserve choices and existing SPDX operators."""

    def test_cargo_legacy_separator_becomes_spdx_or(self):
        self.assertEqual(SBOM.spdx_expression("MIT/Apache-2.0"), "MIT OR Apache-2.0")
        self.assertEqual(SBOM.spdx_expression("MIT / Apache-2.0"), "MIT OR Apache-2.0")

    def test_existing_spdx_expression_is_unchanged(self):
        expression = "(MIT OR Apache-2.0) AND BSD-3-Clause"
        self.assertEqual(SBOM.spdx_expression(expression), expression)
        self.assertEqual(SBOM.spdx_expression("GPL-3.0-only WITH Classpath-exception-2.0"), "GPL-3.0-only WITH Classpath-exception-2.0")


if __name__ == "__main__":
    unittest.main()
