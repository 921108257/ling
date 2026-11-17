"""0.3 流式证据分析的窗口与不确定性契约。
Contracts for windows and uncertainty in the streaming 0.3 evidence analyzer.
"""
import gzip
import json
from pathlib import Path
import tempfile
import unittest

from v030_analyze import GROUPS, JsonLines, Diagnostics, analyze_phase, main


def value(data):
    """独立构造 INFO 包装；Construct INFO wrappers independently of the producer."""
    return {"type": "Map" if isinstance(data, dict) else "List" if isinstance(data, list) else "UInt", "value": data}


def sample(start, completed=None, count=None, kind="periodic"):
    """默认每秒十次，四组独立累计；Default to ten counts per second for four independent groups."""
    completed = start + .1 if completed is None else completed
    count = int(start * 10) if count is None else count
    consumer = {"confirmed_watermarks": {str(shard): str(count if shard == 0 else 0) for shard in range(4)},
                "confirmed_above_watermark": {str(shard): 0 for shard in range(4)},
                "active_seconds": start, "termination": "running"}
    return {"sample_started_elapsed_seconds": start, "sample_completed_elapsed_seconds": completed, "sample_kind": kind,
            "workload": {"counts": {"worker_0_cycles": count, "worker_1_cycles": count * 2, "producer_write_confirmations": count * 3}},
            "consumers": {group: {**consumer} for group in GROUPS},
            "info": value({"events": value({"committed": value(str(count)),
                                              "by_kind": value([value(str(count))] + [value("0") for _ in range(5)]),
                                              "journal_bytes": value(str(count * 10))})}),
            "journal": {"bytes": count * 20, "truncated": False}}


class Analyzer(unittest.TestCase):
    """只操作临时证据文件，不启动服务；Use temporary evidence only, with no service process."""
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.phase = self.root / "reliable-shadow-noeviction"
        self.phase.mkdir()
        self.result = {"event_mode": "reliable", "phase": "shadow", "policy": "noeviction", "status": "duration_reached",
                       "requested_seconds": 120, "load_started_monotonic_seconds": 1000,
                       "load_stop_requested_elapsed_seconds": 125.2, "elapsed_seconds": 130,
                       "load_start": sample(0), "load_stop": sample(125), "consumer_event_samples": 0,
                       "consumers": {group: {"planned_disconnected_seconds": 0, "planned_offline": False, "reconnects": 0} for group in GROUPS},
                       "hello": value({"stream_id": value({"hex": "ab" * 16})}), "findings": {}, "server_errors": {}}
        for boundary in ("load_start", "load_stop"):
            del self.result[boundary]["info"]
        self.rows = [sample(index) for index in range(5, 126, 5)] + [sample(130, count=99999, kind="terminal")]
        self.errors, self.events = [], []
        (self.root / "environment.json").write_text(json.dumps({"settings": {"workers": 2, "warmup_seconds": 0}}), encoding="utf-8")

    def write(self):
        """gzip 空文件也属于完整输入；Empty gzip files are complete inputs too."""
        (self.phase / "result.json").write_text(json.dumps(self.result), encoding="utf-8")
        for name, rows in (("resources.jsonl.gz", self.rows), ("errors.jsonl.gz", self.errors), ("consumer-events.jsonl.gz", self.events)):
            with gzip.open(self.phase / name, "wt", encoding="utf-8") as stream:
                for row in rows:
                    stream.write(json.dumps(row) + "\n")

    def analyze(self):
        """测试仅缓存少量窗口，正式 CLI 即时写盘；Tests collect few windows; the CLI streams to disk."""
        self.write()
        windows = []
        return windows, analyze_phase(self.phase, windows.append)

    def test_wholly_contained_counts_and_partial_window_are_separate(self):
        """跨边界样本不用插值，terminal 不加入有效负载。
        Never interpolate crossing samples or add terminal counts to effective load.
        """
        self.rows.insert(11, sample(59.9, 60.2))
        windows, summary = self.analyze()
        self.assertEqual(len(windows), 3)
        self.assertEqual((summary["full_windows"], summary["partial_windows"]), (2, 1))
        self.assertEqual(windows[0]["metrics"]["worker_0_cycles"]["delta"], "550")
        self.assertEqual(windows[0]["metrics"]["committed"]["delta"], "500")
        self.assertAlmostEqual(windows[0]["metrics"]["committed"]["observed_span_seconds"], 49.9)
        self.assertEqual(windows[0]["crossing_samples_excluded"], 1)
        self.assertGreater(windows[1]["crossing_samples_excluded"], 0)
        self.assertEqual(windows[1]["metrics"]["worker_0_cycles"]["delta"], "550")
        self.assertFalse(windows[2]["full_window"])
        self.assertEqual(summary["observed_minima"]["worker_0_cycles"]["value"], "550")
        self.assertEqual(summary["journal_sampled_peaks"]["journal_active_bytes"]["sample_kind"], "terminal")
        self.assertEqual(summary["diagnostic_elapsed_seconds"], 130)

    def test_missing_counter_remains_unknown_and_does_not_hide_other_workers(self):
        """缺失不是零，单个 worker 不由聚合值掩盖；Missing is not zero and workers remain independent."""
        del self.rows[1]["workload"]["counts"]["worker_1_cycles"]
        windows, summary = self.analyze()
        metric = windows[0]["metrics"]["worker_1_cycles"]
        self.assertIsNone(metric["delta"])
        self.assertEqual((metric["unavailable_reason"], metric["missing_samples"]), ("missing_samples", 1))
        self.assertEqual(windows[0]["metrics"]["worker_0_cycles"]["delta"], "550")
        self.assertEqual(summary["observed_minima"]["worker_1_cycles"]["usable_full_windows"], 1)

    def test_regression_is_not_clamped_and_invalidates_later_deltas(self):
        """回退不能 clamp 到零或重设基线；Regressions cannot be clamped or reset into valid progress."""
        self.rows[1]["workload"]["counts"]["worker_0_cycles"] = 40
        windows, summary = self.analyze()
        self.assertEqual(windows[0]["metrics"]["worker_0_cycles"]["unavailable_reason"], "counter_regressed")
        self.assertIsNone(windows[1]["metrics"]["worker_0_cycles"]["delta"])
        self.assertEqual(summary["diagnostics"]["counts"]["counter_regressed"], 1)
        self.assertIsNone(summary["observed_minima"]["worker_0_cycles"]["value"])
        self.assertEqual(summary["observed_minima"]["worker_0_cycles"]["unavailable_full_windows"], 2)

    def test_offline_intersection_spans_windows_and_preserves_active_coverage(self):
        """45 到 75 秒离线在两个整窗各占十五秒；An offline interval from 45 to 75 intersects each window by fifteen seconds."""
        self.errors = [{"category": "planned_consumer_disconnect_started", "consumer": "slow-db0", "monotonic_seconds": 1045},
                       {"category": "planned_consumer_disconnect_completed", "consumer": "slow-db0", "started_monotonic_seconds": 1045,
                        "completed_monotonic_seconds": 1075, "duration_seconds": 30, "reconnected": True}]
        self.result["consumers"]["slow-db0"].update(planned_disconnected_seconds=30, reconnects=1)
        for row in [self.result["load_start"], self.result["load_stop"], *self.rows]:
            elapsed = row["sample_started_elapsed_seconds"]
            row["consumers"]["slow-db0"]["active_seconds"] = elapsed - max(0, min(elapsed, 75) - 45)
        windows, summary = self.analyze()
        self.assertEqual([row["consumers"]["slow-db0"]["planned_offline_seconds"] for row in windows], [15, 15, 0])
        self.assertEqual(windows[0]["consumers"]["slow-db0"]["planned_online_seconds"], 45)
        self.assertEqual(windows[0]["consumers"]["slow-db0"]["observed_active_seconds"], 45)
        self.assertEqual(windows[0]["consumers"]["slow-db0"]["active_coverage_ratio"], 1)
        self.assertTrue(summary["offline"]["timeline_complete"])

    def test_unmatched_offline_and_corrupt_gzip_are_visible(self):
        """不完整离线与损坏输入不能静默成为完整覆盖；Incomplete intervals and corrupt inputs are explicit."""
        self.errors = [{"category": "planned_consumer_disconnect_started", "consumer": "slow-db0", "monotonic_seconds": 1045}]
        self.write()
        (self.phase / "consumer-events.jsonl.gz").write_bytes(b"not-gzip")
        windows = []
        summary = analyze_phase(self.phase, windows.append)
        self.assertFalse(summary["offline"]["timeline_complete"])
        self.assertFalse(summary["source_files"]["consumer-events.jsonl.gz"]["complete"])
        self.assertEqual(summary["diagnostics"]["counts"]["unclosed_offline_interval"], 1)
        self.assertFalse(windows[0]["offline_timeline_complete"])
        self.assertIsNone(windows[0]["consumers"]["slow-db0"]["planned_offline_seconds"])

    def test_offline_order_is_required_per_group_without_global_timestamp_assumption(self):
        """不同组在日志锁前取时可能交错，每组自身仍必须顺序配对。
        Groups may interleave timestamps captured before the log lock; each group must still pair in order.
        """
        self.errors = [{"category": "planned_consumer_disconnect_started", "consumer": "slow-db0", "monotonic_seconds": 1045},
                       {"category": "planned_consumer_disconnect_started", "consumer": "slow-db1", "monotonic_seconds": 1040},
                       {"category": "planned_consumer_disconnect_completed", "consumer": "slow-db1", "started_monotonic_seconds": 1040,
                        "completed_monotonic_seconds": 1070, "duration_seconds": 30, "reconnected": True},
                       {"category": "planned_consumer_disconnect_completed", "consumer": "slow-db0", "started_monotonic_seconds": 1045,
                        "completed_monotonic_seconds": 1075, "duration_seconds": 30, "reconnected": True}]
        for group in ("slow-db0", "slow-db1"):
            self.result["consumers"][group].update(planned_disconnected_seconds=30, reconnects=1)
        windows, summary = self.analyze()
        self.assertTrue(summary["offline"]["timeline_complete"])
        self.assertEqual(windows[0]["consumers"]["slow-db1"]["planned_offline_seconds"], 20)
        self.assertEqual(windows[1]["consumers"]["slow-db1"]["planned_offline_seconds"], 10)

    def test_resource_trend_uses_phase_warmup_and_terminal_at_actual_times(self):
        """独立预热排除初始峰值，终止样本纳入相同原始拟合。
        Per-phase warmup excludes the initial peak and terminal samples enter the same raw-data fit.
        """
        (self.root / "environment.json").write_text(json.dumps({"settings": {"workers": 2, "warmup_seconds": 30}}), encoding="utf-8")
        for row in self.rows:
            elapsed = row["sample_started_elapsed_seconds"]
            row["native"] = {"rss_bytes": 100 + elapsed * 2, "handles": 4, "threads": 2}
        self.rows[0]["native"]["rss_bytes"] = 999999
        del self.rows[6]["native"]["handles"]
        _, summary = self.analyze()
        trend = summary["resource_trends_recomputed"]
        self.assertEqual(trend["warmup_seconds"], 30)
        self.assertEqual(trend["fields"]["rss_bytes"]["samples"], 21)
        self.assertAlmostEqual(trend["fields"]["rss_bytes"]["slope_per_hour"], 7200)
        self.assertEqual(trend["fields"]["rss_bytes"]["peak"], 360)
        self.assertEqual(trend["fields"]["handles"]["missing_samples"], 1)
        self.assertEqual(summary["native_availability"]["cpu_seconds"]["missing"], len(self.rows))

    def test_missing_complete_offline_pair_is_detected_from_final_totals(self):
        """日志即使可解压，遗漏整对区间也不能声称完整。
        A readable log that lost an entire interval pair is still incomplete.
        """
        self.result["consumers"]["slow-db1"].update(planned_disconnected_seconds=.2, reconnects=1)
        windows, summary = self.analyze()
        self.assertFalse(summary["offline"]["timeline_complete"])
        self.assertIsNone(windows[1]["consumers"]["slow-db0"]["planned_online_seconds"])
        self.assertEqual(summary["diagnostics"]["counts"]["offline_consumer_totals_mismatch_or_missing"], 1)

    def test_omitted_resources_and_journal_failures_remain_visible(self):
        """采样省略与 Journal 错误不可被资源峰值摘要隐藏。
        Resource peak summaries cannot hide omitted samples or Journal errors.
        """
        self.result["counts"] = {"resource_samples_omitted": 2}
        for index, row in enumerate(self.rows):
            row["info"]["value"]["events"]["value"]["journal_errors"] = value(str(index))
        _, summary = self.analyze()
        self.assertEqual(summary["resource_samples_omitted"], "2")
        self.assertEqual(summary["diagnostics"]["counts"]["resource_samples_omitted_or_unknown"], 1)
        self.assertEqual(summary["info_event_counter_observations"]["journal_errors"]["max"], str(len(self.rows) - 1))

    def test_missing_bracket_has_no_elapsed_fallback(self):
        """旧 elapsed 不足以定位计数读取区间；Legacy elapsed alone cannot locate a counter read."""
        self.rows[0] = {"elapsed_seconds": 5, "sample_kind": "periodic"}
        windows, summary = self.analyze()
        self.assertEqual(summary["diagnostics"]["counts"]["invalid_sample_bracket"], 1)
        self.assertEqual(windows[0]["metrics"]["committed"]["delta"], "450")

    def test_exact_boundary_snapshot_can_close_and_open_neighboring_windows(self):
        """零时长边界是共同端点，不会把增量重复算入两窗。
        A zero-duration boundary can serve adjacent endpoints without duplicating any increment.
        """
        self.rows = [sample(5, 5), sample(60, 60), sample(115, 115), sample(125, 125)]
        windows, _ = self.analyze()
        self.assertEqual(windows[0]["metrics"]["committed"]["delta"], "550")
        self.assertEqual(windows[1]["metrics"]["committed"]["delta"], "550")
        self.assertEqual(windows[0]["max_sample_start_gap_seconds"], 55)

    def test_one_long_sample_is_excluded_from_every_window_it_crosses(self):
        """跨越两个窗的慢采样不变成任一窗口的计数端点。
        A slow sample crossing two windows cannot become an endpoint in either window.
        """
        self.rows = [sample(5), sample(50, 121, count=999), sample(125)]
        windows, _ = self.analyze()
        self.assertEqual([row["crossing_samples_excluded"] for row in windows], [1, 1, 1])
        self.assertIsNone(windows[0]["metrics"]["committed"]["delta"])
        self.assertIsNone(windows[1]["metrics"]["committed"]["delta"])

    def test_unique_ack_uses_watermarks_and_holes_without_float_rounding(self):
        """大整数与空洞累加不经 ACK 调用数或浮点；Large integers and holes bypass ACK call counts and floats."""
        for row in self.rows:
            count = int(row["sample_started_elapsed_seconds"])
            group = row["consumers"]["fast-db1"]
            group["acks"] = 999999
            group["confirmed_watermarks"] = {str(shard): str(2**60 + count if shard == 0 else 0) for shard in range(4)}
            group["confirmed_above_watermark"] = {str(shard): 3 if shard == 1 else 0 for shard in range(4)}
        windows, _ = self.analyze()
        self.assertEqual(windows[1]["metrics"]["ack_unique.fast-db1"]["delta"], "55")

    def test_sampled_consumer_events_validate_identity_without_claiming_unique_totals(self):
        """普通事件抽样核对身份，类型错误不能充作已验证事件。
        Validate sampled identities without treating malformed types as verified events or unique totals.
        """
        event = {"category": "event_sample", "consumer": "fast-db1", "database": 1, "shard": 0,
                 "sequence": str(2**60), "stream_id": "ab" * 16, "event_type": "KEY_CREATED"}
        self.events = [event, {**event, "database": True}]
        self.result["consumer_event_samples"] = 2
        _, summary = self.analyze()
        self.assertEqual(summary["consumer_event_samples"]["sample_counts"], {"fast-db1": 1})
        self.assertEqual(summary["diagnostics"]["counts"]["invalid_consumer_event_sample"], 1)

    def test_cli_expands_root_and_streams_valid_json(self):
        """输出文件可直接供门槛审阅且不含通过结论；The JSON report supports review without a pass decision."""
        self.write()
        output = self.root / "analysis.json"
        self.assertEqual(main([str(self.root), "--output", str(output)]), 0)
        report = json.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(report["format"], "ling-v030-observations-1")
        self.assertEqual(len(report["phases"][0]["windows"]), 3)
        self.assertNotIn("passed", report["phases"][0]["summary"])

    def test_jsonl_rejects_nonobjects_and_unterminated_line(self):
        """坏行之后仍读后续证据，但文件完整性为 false；Continue after bad JSON, retaining incomplete integrity."""
        path = self.root / "rows.gz"
        with gzip.open(path, "wb") as stream:
            stream.write(b'[]\n{"n":1}')
        diagnostics = Diagnostics()
        reader = JsonLines(path, diagnostics)
        self.assertEqual(list(reader.rows()), [{"n": 1}])
        self.assertFalse(reader.stats["complete"])
        self.assertEqual(set(diagnostics.counts), {"invalid_json_line", "unterminated_json_line"})

    def test_truncated_gzip_footer_preserves_rows_and_marks_incomplete(self):
        """已有行可分析，但 gzip 尾部损坏必须显式可见。
        Earlier rows remain readable while a damaged gzip footer is explicit.
        """
        path = self.root / "truncated.gz"
        with gzip.open(path, "wb") as stream:
            stream.write(b'{"n":1}\n')
        path.write_bytes(path.read_bytes()[:-5])
        diagnostics = Diagnostics()
        reader = JsonLines(path, diagnostics)
        self.assertEqual(list(reader.rows()), [{"n": 1}])
        self.assertFalse(reader.stats["complete"])
        self.assertEqual(diagnostics.counts["unreadable_gzip"], 1)


if __name__ == "__main__":
    unittest.main()
