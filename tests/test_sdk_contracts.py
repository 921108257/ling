"""驱动不能把漏跑、重复或跳过当作成功；missing, duplicate, and skipped fixtures cannot pass."""

import importlib.util
import contextlib
import io
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("sdk_contracts", Path(__file__).with_name("sdk_contracts.py"))
CONTRACTS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONTRACTS)


class ContractAccountingTests(unittest.TestCase):
    """独立合成输出检查 ID 集合，不只比较总数。
    Check ID sets with independent synthetic output instead of comparing totals alone.
    """

    def setUp(self):
        self.plan = {("values", "null"), ("values", "uint_max")}

    def test_exact_ids_pass(self):
        counts, errors = CONTRACTS.inspect_reports(
            "noise\nLING_CONTRACT\tvalues\tnull\tpassed\nLING_CONTRACT\tvalues\tuint_max\tpassed\n", self.plan)
        self.assertFalse(errors)
        self.assertEqual(counts, dict(planned=2, executed=2, passed=2, failed=0, skipped=0, missing=0))

    def test_duplicate_cannot_hide_missing_id(self):
        _, errors = CONTRACTS.inspect_reports("LING_CONTRACT\tvalues\tnull\tpassed\n" * 2, self.plan)
        self.assertTrue(any("duplicate" in error for error in errors))
        self.assertTrue(any("missing" in error for error in errors))

    def test_zero_exit_without_reports_is_not_acceptance(self):
        counts, errors = CONTRACTS.inspect_reports("all tests passed\n", self.plan)
        self.assertEqual(counts["executed"], 0)
        self.assertEqual(counts["missing"], 2)
        self.assertTrue(errors)

    def test_failure_skip_extra_and_malformed_are_rejected(self):
        for report in ("values\tnull\tfailed", "values\tnull\tskipped", "values\textra\tpassed", "values\tnull\tok"):
            with self.subTest(report=report):
                _, errors = CONTRACTS.inspect_reports("LING_CONTRACT\t" + report, self.plan)
                self.assertTrue(errors)

    def test_duplicate_fixture_ids_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "sessions.tsv").write_text("same|request\nsame|batch\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "duplicate"):
                CONTRACTS.fixture_plan(root, sessions=True)

    def test_real_fixture_plan_has_all_info_field_ids(self):
        plan = CONTRACTS.fixture_plan()
        self.assertIn(("info-fields", "stats.execution_queued"), plan)
        self.assertIn(("commands", "missing"), plan)
        self.assertIn(("live-values", "uint_max"), plan)

    def test_process_success_cannot_hide_missing_reports(self):
        """退出码零不能补足未执行的夹具；exit zero cannot fill in unexecuted fixtures."""
        table = []
        with patch.object(CONTRACTS, "execute", return_value=SimpleNamespace(stdout="ok", returncode=0)):
            with self.assertRaisesRegex(RuntimeError, "missing reports"):
                CONTRACTS.contracts("Go", "TCP", [], self.plan, table)
        self.assertEqual(table[0][2]["executed"], 0)

    def test_summary_lists_paths_that_never_started(self):
        """首个失败不能隐藏后续整条路径；an early failure cannot hide later unstarted paths."""
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            CONTRACTS.print_counts([], 2, 3)
        self.assertIn("Rust | local-peer | 3 | 0 | 0 | 0 | 3", output.getvalue())
        self.assertIn("Java | mutual TLS | 2 | 0 | 0 | 0 | 2", output.getvalue())


if __name__ == "__main__":
    unittest.main()
