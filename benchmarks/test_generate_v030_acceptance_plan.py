"""轻量虚构夹具；Small synthetic fixtures, never read real calibration or start a subprocess."""
from fractions import Fraction
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("v030_plan_generator", Path(__file__).with_name("generate-v030-acceptance-plan.py"))
PLAN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PLAN)


def resources(rate=1000, initial=100, peak=200):
    """精确、极小容量观察；Exact tiny capacity observations."""
    return {"capacity_scenarios": {"prerequisites_for_emitting_scenarios": {"complete": True},
                "initial_active_bytes": str(initial), "observed_active_peak_bytes": str(peak)},
            "counter_observations": {"journal_transaction_bytes.sum": {"rate_unavailable_reasons": [],
                "delta": str(rate * 600), "sample_start_span_seconds": 600,
                "positive_max_adjacent_start_normalized": {"delta": str(rate * 5), "start_span_seconds": 5}}},
            "fixed_60s_summary": {"maximum_lower_bounds": {"journal_transaction_bytes.sum": {"delta_lower_bound": str(rate * 60)}}}}


class ScalingTests(unittest.TestCase):
    """整数与时间边界；Integer and timing boundaries."""
    def test_u64_delta_never_rounds_through_float(self):
        count, elapsed, duration = 2**64 - 1, 600.0000001, 259500
        expected = Fraction(4, 5) * count / Fraction("600.0000001") * duration
        exact_ceil = -(-expected.numerator // expected.denominator)
        self.assertEqual(PLAN.scaled_total(str(count), elapsed, duration), exact_ceil)

    def test_total_uses_elapsed_instead_of_requested_calibration(self):
        self.assertEqual(PLAN.scaled_total(100, 625, 1000), 128)

    def test_window_uses_counter_lower_bound_without_span(self):
        self.assertEqual(PLAN.scaled_window("6"), 5)

    def test_zero_and_missing_baselines_refused(self):
        for value in (0, None, True):
            with self.subTest(value=value), self.assertRaises(ValueError):
                PLAN.scaled_window(value)

    def test_millisecond_rounding_is_upward(self):
        self.assertEqual(PLAN.ceil_milliseconds(Fraction("5.0000001") * Fraction(5, 4)), 6.251)

    def test_four_in_600_boundary_covers_432_in_formal_phase(self):
        budget = PLAN.offline_budget(4, .8, 600, 51840, .2)
        self.assertEqual((budget["min_reconnects"], budget["max_reconnects"]), (276, 433))
        self.assertLessEqual(432, budget["max_reconnects"])
        self.assertAlmostEqual(budget["max_seconds"], 86.6)

    def test_no_slow_offline_evidence_refused(self):
        with self.assertRaises(ValueError):
            PLAN.offline_budget(0, 0, 600, 51840, 0)


class CapacityTests(unittest.TestCase):
    """历史情景与准入边界；Historical scenarios and admission boundaries."""
    def test_record_and_checkpoint_reserve_are_explicit(self):
        value = PLAN.capacity_projection(resources(), 259500)
        self.assertEqual(value["event_total_active_admission_bytes"], "1610612736")
        self.assertEqual(value["maximum_record_reserve_bytes"], str(2097152 + 16))
        self.assertEqual(value["topology_checkpoint_reserve_bytes"], "289")

    def test_peak_is_not_discarded_by_low_rate(self):
        value = PLAN.capacity_projection(resources(rate=1, peak=900000), 51840)
        self.assertEqual(value["projections"]["51840"]["active_bytes_historical_scenario"], "900000")

    def test_full_run_without_reclamation_can_exceed_limit_without_rejecting_first_day(self):
        value = PLAN.capacity_projection(resources(rate=7000), 259500)
        self.assertTrue(value["startup_scenario_below_event_admission"])
        self.assertLess(int(value["projections"]["259500"]["remaining_to_event_admission_bytes"]), 0)

    def test_first_day_over_limit_is_blocking(self):
        self.assertFalse(PLAN.capacity_projection(resources(rate=20000), 259500)["startup_scenario_below_event_admission"])

    def test_reserve_exactly_at_admission_is_rejected(self):
        reserve, threshold = 2097152 + 16 + 289, 1610612736
        value = PLAN.capacity_projection(resources(rate=1, peak=threshold - reserve), 51840)
        self.assertFalse(value["startup_scenario_below_event_admission"])
        self.assertEqual(value["projections"]["51840"]["remaining_to_event_admission_bytes"], "0")

    def test_unavailable_capacity_evidence_refused(self):
        fixture = resources()
        fixture["capacity_scenarios"]["prerequisites_for_emitting_scenarios"]["complete"] = False
        with self.assertRaises(ValueError):
            PLAN.capacity_projection(fixture, 51840)

    def test_maximum_observed_rate_is_selected(self):
        fixture = resources(rate=1)
        fixture["counter_observations"]["journal_transaction_bytes.sum"]["positive_max_adjacent_start_normalized"] = {
            "delta": "21", "start_span_seconds": 2}
        value = PLAN.capacity_projection(fixture, 51840)
        self.assertEqual(value["selected_rate_exact"], "21/2")


class ArchiveTests(unittest.TestCase):
    """极小本地夹具仅验证摘要绑定；Tiny local fixtures verify digest binding only."""
    def test_original_changed_after_archive_is_refused(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            directory = Path(temporary).resolve()
            raw, archive = directory / "calibration", directory / "archive"
            raw.mkdir()
            archive.mkdir()
            original = raw / "result.json"
            original.write_text('{"count": 1}', encoding="utf-8")
            data = original.read_bytes()
            inventory = {"source_root": str(raw), "files": [{"path": "raw/result.json", "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest(), "storage": "archive", "archive": "raw-001.tar.gz"}], "exact_binary_aliases": []}
            listing = archive / "evidence-files.manifest.json"
            listing.write_text(json.dumps(inventory), encoding="utf-8")
            for name in ("summary.json", "archive-verification.json", "analysis.json", "extra.json",
                         "v030_analyze.py", "analyze-v030-calibration-extra.py"):
                (archive / name).write_text("{}", encoding="utf-8")
            (archive / "raw-001.tar.gz").write_bytes(b"synthetic archive bytes; no real evidence")
            manifest = {"run_id": raw.name, "git_path_filters_preserve_bytes": True, "files": [{"path": item.name,
                "bytes": item.stat().st_size, "sha256": hashlib.sha256(item.read_bytes()).hexdigest()} for item in archive.iterdir()]}
            (archive / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
            _, bind, recheck = PLAN.archive_sources(archive, raw)
            self.assertEqual(bind(original), {"count": 1})
            original.write_text('{"count": 2}', encoding="utf-8")
            with self.assertRaises(ValueError):
                recheck()

    def test_required_archive_sources_cannot_be_omitted(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as temporary:
            directory = Path(temporary).resolve()
            (directory / "manifest.json").write_text(json.dumps({"run_id": "raw", "files": [],
                "git_path_filters_preserve_bytes": True}), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "required archive sources"):
                PLAN.archive_sources(directory, directory / "raw")


if __name__ == "__main__":
    unittest.main()
