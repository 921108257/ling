"""数值核对器的小型边界夹具；Small boundary fixtures for the numerical evaluator."""
import copy
import gzip
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("evaluate-v030-acceptance.py")
SPEC = importlib.util.spec_from_file_location("evaluate_v030_acceptance", SCRIPT)
E = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(E)


def consumer(active=0, offline=0, reconnects=0, termination="running"):
    """生成固定身份与唯一ACK水位；Build fixed consumer identity and unique ACK watermarks."""
    return {"active_seconds": active, "planned_disconnected_seconds": offline, "reconnects": reconnects,
            "termination": termination, "errors": {}, "started_monotonic_seconds": 99.8,
            "elapsed_seconds": active + offline, "planned_offline": False,
            "confirmed_watermarks": {str(i): "0" for i in range(4)},
            "confirmed_above_watermark": {str(i): 0 for i in range(4)}, "received": 999999, "acks": 999999, "duplicates": 0}


def boundary_fixture():
    """最终累计数故意远大于负载内计数；Final counters deliberately exceed load-boundary counters."""
    start = {"sample_started_elapsed_seconds": 0, "sample_completed_elapsed_seconds": .02,
             "workload": {"counts": {"worker_0_cycles": 3, "worker_1_cycles": 5,
                                      "producer_write_confirmations": 7, "pressure_cycles": 8}},
             "consumers": {group: consumer(.2) for group in E.GROUPS}}
    stop = {"sample_started_elapsed_seconds": 120, "sample_completed_elapsed_seconds": 120.02,
            "workload": {"counts": {"worker_0_cycles": 13, "worker_1_cycles": 25,
                                     "producer_write_confirmations": 37, "pressure_cycles": 38}}, "consumers": {}}
    final = {}
    budgets = {}
    for group in E.GROUPS:
        slow = group.startswith("slow-")
        stop["consumers"][group] = consumer(119.8 if slow else 120.2, .4 if slow else 0, 2 if slow else 0)
        stop["consumers"][group]["confirmed_watermarks"]["0"] = "11"
        final[group] = copy.deepcopy(stop["consumers"][group])
        final[group].update(termination="requested_stop", elapsed_seconds=120.5)
        budgets[group] = {"min_reconnects": 2 if slow else 0, "max_reconnects": 2 if slow else 0,
                          "min_seconds": .4 if slow else 0, "max_seconds": .4 if slow else 0}
    totals, window = E.metric_sets(2)
    phase = {"phase": "pressure", "minimum_totals": {name: 1 for name in totals},
             "minimum_window": {name: 1 for name in window}, "offline": budgets,
             "minimum_window_active_seconds": dict.fromkeys(E.GROUPS, 40)}
    result = {"load_start": start, "load_stop": stop, "load_started_monotonic_seconds": 100,
              "load_stop_requested_elapsed_seconds": 120, "consumers": final,
              "counts": {"worker_0_cycles": 999999, "producer_write_confirmations": 999999}}
    return phase, result


def window_fixture():
    """两个完整窗口，每组均有独立计数与活跃下界；Two full windows with independent counts and active lower bounds."""
    phase, result = boundary_fixture()
    windows = []
    for index in range(2):
        metrics = {name: {"delta": "10", "unavailable_reason": None, "missing_samples": 0}
                   for name in phase["minimum_window"]}
        metrics["by_kind.Evicted"] = {"delta": "0"}
        for group in E.GROUPS:
            metrics["active_seconds." + group] = {"delta": 55.0}
        windows.append({"begin_seconds": index * 60, "end_seconds": (index + 1) * 60,
                        "full_window": True, "offline_timeline_complete": True, "observed_span_seconds": 50,
                        "metrics": metrics, "max_sample_start_gap_seconds": 5.1, "max_sample_duration_seconds": .1,
                        "consumers": {group: {"sampled_termination_states": {"running": 12}, "planned_offline_seconds": 0}
                                      for group in E.GROUPS}})
    summary = {"full_windows": 2, "partial_windows": 0,
               "observed_minima": {name: {"usable_full_windows": 2, "unavailable_full_windows": 0}
                                   for name in phase["minimum_window"]}}
    return phase, result, {"windows": windows, "summary": summary}, {"max_sample_start_gap_seconds": 6, "max_sample_duration_seconds": 1}


def resource_fixture():
    """零资源斜率为合法的稳定观测；Zero resource slopes are valid steady-state observations."""
    summary = {"resource_rows": 100, "resource_samples_omitted": "0",
               "native_availability": {name: {"valid": 100, "missing": 0} for name in ("cpu_seconds", "rss_bytes", "handles", "threads")},
               "max_resource_start_gap_seconds": 5.1, "max_resource_sample_duration_seconds": .1,
               "resource_trends_recomputed": {"warmup_seconds": 3600, "fields": {
                   name: {"samples": 10, "missing_samples": 0, "span_seconds": 90,
                          "max_sample_start_gap_seconds": 5.1, "slope_per_hour": 0}
                   for name in ("rss_bytes", "handles", "threads")}}}
    limits = {"warmup_seconds": 3600, "max_sample_start_gap_seconds": 6, "max_sample_duration_seconds": 1,
              "rss_bytes_per_hour": 1048576, "handles_per_hour": .1, "threads_per_hour": .1}
    return summary, limits


def raw_resources():
    """与分析行数独立保存的驱动计数；Driver counts independently retained from analyzed rows."""
    return {"resource_samples_saved": 100, "native_resources": {"attempts": 100, "missing": {},
            "valid": dict.fromkeys(("cpu_seconds", "rss_bytes", "handles", "threads"), 100)}}


class BoundaryTests(unittest.TestCase):
    """验证不可由排空与累计调用数补证；Drain and cumulative calls cannot replace load evidence."""
    def test_totals_subtract_start_and_ignore_post_stop(self):
        phase, result = boundary_fixture()
        totals = E.validate_totals(E.analyzer_module(), phase, result, 2)
        self.assertEqual(totals["worker_0_cycles"], "10")
        self.assertEqual(totals["producer_write_confirmations"], "30")
        self.assertEqual(totals["ack_unique.fast-db0"], "11")
        phase["minimum_totals"]["worker_0_cycles"] = 11
        with self.assertRaisesRegex(ValueError, "total below"):
            E.validate_totals(E.analyzer_module(), phase, result, 2)

    def test_ack_calls_do_not_replace_unique_watermarks(self):
        phase, result = boundary_fixture()
        del result["load_stop"]["consumers"]["slow-db1"]["confirmed_watermarks"]
        with self.assertRaisesRegex(ValueError, "integer"):
            E.validate_totals(E.analyzer_module(), phase, result, 2)

    def test_whole_phase_coverage_and_snapshot_bracket(self):
        phase, result = boundary_fixture()
        E.validate_consumers(phase, result)
        result["load_stop"]["consumers"]["slow-db0"]["active_seconds"] -= 10
        with self.assertRaisesRegex(ValueError, "snapshot interval"):
            E.validate_consumers(phase, result)

    def test_large_final_elapsed_cannot_hide_early_termination(self):
        phase, result = boundary_fixture()
        result["consumers"]["slow-db0"]["elapsed_seconds"] = 999999
        result["load_stop"]["consumers"]["slow-db0"]["termination"] = "expected_fast_lag"
        with self.assertRaisesRegex(ValueError, "stopped early"):
            E.validate_consumers(phase, result)

    def test_final_ack_counts_are_checked_directly(self):
        phase, result = boundary_fixture()
        result["consumers"]["fast-db0"]["acks"] -= 1
        with self.assertRaisesRegex(ValueError, "not fully confirmed"):
            E.validate_consumers(phase, result)

    def test_missing_window_not_hidden_by_usable_minimum(self):
        phase, result, analyzed, limits = window_fixture()
        E.validate_windows(phase, result, analyzed, limits)
        analyzed["windows"][1]["metrics"]["worker_1_cycles"].update(delta=None, unavailable_reason="missing_samples")
        with self.assertRaisesRegex(ValueError, "unavailable"):
            E.validate_windows(phase, result, analyzed, limits)

    def test_active_window_absolute_and_span_are_separate(self):
        phase, result, analyzed, limits = window_fixture()
        analyzed["windows"][0]["metrics"]["active_seconds.fast-db1"]["delta"] = 45
        with self.assertRaisesRegex(ValueError, "coverage gap"):
            E.validate_windows(phase, result, analyzed, limits)

    def test_zero_slope_valid_but_null_slope_fails(self):
        summary, limits = resource_fixture()
        E.validate_resources(summary, limits, raw_resources())
        summary["resource_trends_recomputed"]["fields"]["rss_bytes"]["slope_per_hour"] = None
        with self.assertRaisesRegex(ValueError, "finite"):
            E.validate_resources(summary, limits, raw_resources())

    def test_missing_native_and_omission_fail(self):
        summary, limits = resource_fixture()
        summary["native_availability"]["handles"]["missing"] = 1
        with self.assertRaisesRegex(ValueError, "unavailable"):
            E.validate_resources(summary, limits, raw_resources())
        summary, limits = resource_fixture()
        summary["resource_samples_omitted"] = "1"
        with self.assertRaisesRegex(ValueError, "omitted"):
            E.validate_resources(summary, limits, raw_resources())

    def test_partial_window_without_samples_is_recorded_not_failed(self):
        phase, result, analyzed, limits = window_fixture()
        result["load_stop_requested_elapsed_seconds"] = 120.001
        partial = copy.deepcopy(analyzed["windows"][-1])
        partial.update(begin_seconds=120, end_seconds=120.001, full_window=False)
        for group in E.GROUPS:
            partial["consumers"][group]["sampled_termination_states"] = {}
        analyzed["windows"].append(partial)
        analyzed["summary"]["partial_windows"] = 1
        E.validate_windows(phase, result, analyzed, limits)
        partial["consumers"]["slow-db0"]["sampled_termination_states"] = {"error": 1}
        with self.assertRaisesRegex(ValueError, "not running"):
            E.validate_windows(phase, result, analyzed, limits)

    def test_saved_resource_count_mismatch_fails(self):
        summary, limits = resource_fixture()
        raw = raw_resources()
        raw["resource_samples_saved"] = 99
        with self.assertRaisesRegex(ValueError, "sample counts differ"):
            E.validate_resources(summary, limits, raw)

    def test_calibration_formula_and_zero_baseline(self):
        phase, _ = boundary_fixture()
        phase.update(directory="fast-pressure/fast-pressure-noeviction", requested_seconds=51840)
        totals = dict.fromkeys(phase["minimum_totals"], 10)
        windows = dict.fromkeys(phase["minimum_window"], 10)
        phase["minimum_totals"] = dict.fromkeys(totals, 692)
        phase["minimum_window"] = dict.fromkeys(windows, 8)
        phase["minimum_window_active_seconds"] = dict.fromkeys(E.GROUPS, 44)
        baseline = {"actual_load_seconds": 600, "total_deltas": totals, "minimum_window": windows,
                    "minimum_window_active_seconds": dict.fromkeys(E.GROUPS, 55)}
        plan = {"calibration": {"factor": .8, "phases": {"fast-pressure-noeviction": baseline}}}
        E.validate_calibration(plan, phase)
        phase["minimum_totals"]["worker_0_cycles"] = 691
        with self.assertRaisesRegex(ValueError, "differs from calibration"):
            E.validate_calibration(plan, phase)
        phase["minimum_totals"]["worker_0_cycles"] = 692
        baseline["minimum_window"]["committed"] = 0
        with self.assertRaisesRegex(ValueError, "zero calibration"):
            E.validate_calibration(plan, phase)

    def test_ttl_first_accepts_either_frozen_capacity_proof(self):
        phase = {"phase": "pressure", "policy": "ttl-first", "capacity": {"minimum_oom": 1, "minimum_evicted": 0}}
        self.assertEqual(E.capacity_thresholds(phase), (1, 0))
        phase["capacity"] = {"minimum_oom": 0, "minimum_evicted": 1}
        self.assertEqual(E.capacity_thresholds(phase), (0, 1))
        phase["capacity"]["minimum_evicted"] = 0
        with self.assertRaisesRegex(ValueError, "capacity evidence"):
            E.capacity_thresholds(phase)

    def test_delivery_observations_do_not_claim_no_loss(self):
        _, result = boundary_fixture()
        result["final_info"] = {"value": {"events": {"value": {
            name: {"value": str(value)} for name, value in {"pending": 200, "dropped": 500, "redelivered": 20, "committed": 1000}.items()}}}}
        for row in result["consumers"].values():
            row.update(received=100, acks=100, duplicates=2)
        observed = E.delivery_observations(E.analyzer_module(), result, {"info_event_counter_observations": {"dropped": {"max": "500"}}})
        self.assertEqual(observed["post_drain_info"]["dropped"], "500")
        self.assertEqual(observed["post_drain_info"]["pending"], "200")
        self.assertIn("not proof", observed["scope"])


class SourceTests(unittest.TestCase):
    """验证报告绑定和完整源路径；Verify report binding and complete recorded source paths."""
    def test_full_source_root_binding_survives_copy_but_rejects_other_parent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "tool_source").mkdir()
            for name in ("run-v030-layout.py", "v030.py", "v020.py"):
                (root / "tool_source" / name).write_bytes(b"fixed fixture\n")
            digest = E.sha256(root / "tool_source/v030.py")
            plan = {"source_root": "/root/acceptance/formal-run", "run_id": "formal-run", "binary": {"sha256": "a", "revision": "r"},
                    "layout_sha256": digest, "instances": {name: {"script_sha256": digest, "v020_sha256": digest} for name in E.INSTANCES}}
            layout = {"commands": [{"name": name, "command": ["python", "v030.py", "--binary-revision", "r",
                                  plan["source_root"] + "/" + name, "--event-mode", name.split("-")[0]]} for name in E.INSTANCES],
                      "binary_sha256": "a", "binary_revision": "r", "script_and_tool_sha256": {
                          "/original/tools/" + name: digest for name in ("run-v030-layout.py", "v030.py", "v020.py")}}
            E.validate_source_paths(layout, plan, root)
            layout["commands"][0]["command"][4] = "/other/formal-run/" + layout["commands"][0]["name"]
            with self.assertRaisesRegex(ValueError, "output path"):
                E.validate_source_paths(layout, plan, root)

    def test_report_recomputed_and_old_report_rejected(self):
        class Base:
            @staticmethod
            def main(arguments):
                Path(arguments[arguments.index("--output") + 1]).write_bytes(b'{"current":true}\n')
                return 0
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            report = root / "report.json"
            plan = {"analysis_directories": ["fast-shadow"], "analysis_workers": None}
            report.write_bytes(b'{"current":true}\n')
            E.bind_analysis(Base, root, plan, report)
            report.write_bytes(b'{"old":true}\n')
            with self.assertRaisesRegex(ValueError, "recomputed"):
                E.bind_analysis(Base, root, plan, report)

    def test_duplicate_json_and_nonfinite_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "metadata.json"
            for text in ('{"a":1,"a":2}', '{"slope":NaN}'):
                path.write_text(text)
                with self.assertRaises(ValueError):
                    E.read_json(path)

    def test_bad_log_crc_is_a_failed_independent_check(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "complete.log.gz"
            path.write_bytes(gzip.compress(b"complete\n")[:-2])
            checks = E.Checks()
            checks.run("log", str(path), lambda: E.validate_log(path))
            self.assertFalse(checks.rows[0]["passed"])

    def test_expected_rejections_and_forbidden_codes(self):
        base = E.analyzer_module()
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "errors.jsonl.gz"
            for phase, code in (("shadow", "ERR_OVERFLOW"), ("pressure", "ERR_KEY_NOT_FOUND"), ("pressure", "ERR_OUT_OF_MEMORY")):
                row = {"category": "server_error", "code": code, "expected": True}
                with gzip.open(path, "wt", encoding="utf-8") as output:
                    output.write(json.dumps(row) + "\n")
                E.validate_error_records(base, path, {"phase": phase}, {"server_errors": {code: 1}})
            for code, expected in (("ERR_OUT_OF_MEMORY", False), ("ERR_JOURNAL_FULL", True), ("ERR_COMMIT_UNKNOWN", True)):
                with gzip.open(path, "wt", encoding="utf-8") as output:
                    output.write(json.dumps({"category": "server_error", "code": code, "expected": expected}) + "\n")
                with self.assertRaisesRegex(ValueError, "forbidden"):
                    E.validate_error_records(base, path, {"phase": "pressure"}, {"server_errors": {code: 1}})

    def test_pressure_phases_must_run_serially_in_policy_order(self):
        raw, intervals = [], []
        for mode, offset in (("fast", 100), ("reliable", 200)):
            for policy, start, duration in [("noeviction", 0, 260000)] + [(policy, offset + index * 51841, 51840) for index, policy in enumerate(E.POLICIES)]:
                phase = "shadow" if start == 0 else "pressure"
                name = f"{mode}-{phase}-{policy}"
                raw.append(({"directory": mode + "-" + phase + "/" + name, "phase": phase},
                            {"load_started_monotonic_seconds": start, "load_stop_requested_elapsed_seconds": duration}))
                intervals.append({"name": name, "phase": phase, "start": start, "end": start + duration})
        layout = {"orchestration_passed": True, "errors": [], "cleanup_events": [], "remaining_processes": [],
                  "driver_exits": [{"name": name, "exit_code": 0} for name in E.INSTANCES],
                  "coverage": {"passed": True, "errors": [], "common_shadow_interval": {"start": 0, "end": 260000}, "intervals": intervals}}
        E.validate_layout(layout, raw)
        raw[2][1]["load_started_monotonic_seconds"] = 100
        intervals[2].update(start=100, end=51940)
        with self.assertRaisesRegex(ValueError, "overlap or run out of order"):
            E.validate_layout(layout, raw)


if __name__ == "__main__":
    unittest.main()
