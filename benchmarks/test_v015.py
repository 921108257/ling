"""基准验收与历史数据契约；Benchmark acceptance and historical-data contracts."""
import copy
import json
from pathlib import Path
import unittest

from v015 import CASES, consumer_policy, cpu_set, metrics_comparison, schedule, summary, validate_result


class Acceptance(unittest.TestCase):
    """错误、早退和缺失不能被生产成功掩盖；Failures cannot hide behind successful producers."""

    def result(self):
        """最小正常持续消费结果；Minimal successful continuous-consumption result."""
        return {"successes": 10, "errors": {}, "warmup_errors": {}, "consumer": "fast",
                "consumption": [4, {}, {}], "consumer_status": {"termination": "requested_stop"}}

    def test_continuous_consumer(self):
        row = self.result()
        policy = consumer_policy("reliable", "slow")
        self.assertTrue(policy["lag_allowed"])
        self.assertFalse(policy["retention_loss_allowed"])
        self.assertTrue(validate_result(row, "reliable")["valid"])
        for field, value in (("consumption", None), ("consumer_status", None),
                             ("consumer_status", {"termination": "error"})):
            bad = copy.deepcopy(row)
            bad[field] = value
            self.assertFalse(validate_result(bad, "reliable")["valid"])

    def test_expected_fast_lag_is_separate(self):
        row = self.result()
        row["consumption"] = [0, {"ERR_OFFSET_EXPIRED: retained offset advanced": 1}, {}]
        row["consumer_status"] = {"termination": "error", "failure_phase": "receive"}
        checked = validate_result(row, "fast")
        self.assertTrue(checked["valid"])
        self.assertEqual(sum(checked["expected_fast_lag"].values()), 1)
        self.assertFalse(validate_result(row, "reliable")["valid"])
        row["consumption"][1]["ERR_JOURNAL_UNAVAILABLE: unavailable"] = 1
        self.assertFalse(validate_result(row, "fast")["valid"])

    def test_no_consumer_and_no_producer_results(self):
        row = self.result()
        row.update(consumer="none", consumption=None, consumer_status=None)
        self.assertTrue(validate_result(row, "off")["valid"])
        row["successes"] = 0
        self.assertFalse(validate_result(row, "off")["valid"])
        self.assertFalse(validate_result(self.result(), "fast", expected_consumer="slow")["valid"])

    def test_schedule_is_complete_and_reproducible(self):
        jobs = schedule(3, 20261117)
        self.assertEqual(len(jobs), 54)
        self.assertEqual(jobs, schedule(3, 20261117))
        self.assertNotEqual(jobs, schedule(3, 20261118))
        self.assertEqual({(j[0], j[3]) for j in jobs},
                         {(name, n) for name, _, _ in CASES for n in range(1, 4)})
        self.assertEqual(len(schedule(3, 1, metrics=True)), 27)
        with self.assertRaises(ValueError):
            schedule(3, 1, "unknown")

    def test_cpu_affinity_rejects_invalid_ranges(self):
        self.assertEqual(cpu_set("0-1,3", [0, 1, 2, 3]), {0, 1, 3})
        for text in ("4", "3-1", "0-1000000", "", "0,unknown"):
            with self.assertRaises(ValueError):
                cpu_set(text, [0, 1, 2, 3])

    def test_metrics_costs_are_separate(self):
        rows = [{"name": f"metrics-get-{variant}", "successes_per_second": rate}
                for variant, rate in (("disabled", 100), ("enabled", 90), ("scraped", 81))]
        result = metrics_comparison(rows)["get"]
        self.assertAlmostEqual(result["collection_throughput_change_percent"], -10)
        self.assertAlmostEqual(result["scraping_additional_throughput_change_percent"], -10)

    def test_existing_stage7_results_keep_raw_errors(self):
        historic = json.loads((Path(__file__).parent / "results/stage7-2026-11-15-results.json").read_text())
        checked = 0
        expected_lag = 0
        for rows in historic.values():
            if not isinstance(rows, list):
                continue
            self.assertTrue(summary(rows))
            for row in rows:
                before = copy.deepcopy(row)
                mode = row.get("event_mode", "off")
                result = validate_result(row, mode, require_status=False)
                self.assertEqual(row, before)
                if consumer_policy(mode, row.get("consumer", "none"))["required"]:
                    checked += 1
                    self.assertFalse(result["termination_verified"])
                    self.assertTrue(result["valid"], result)
                    expected_lag += sum(result["expected_fast_lag"].values())
        self.assertGreater(checked, 0)
        self.assertGreater(expected_lag, 0)


if __name__ == "__main__":
    unittest.main()
