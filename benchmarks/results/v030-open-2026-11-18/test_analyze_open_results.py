"""独立分析器的离线样例；Offline examples for exact open-load evidence accounting."""
from collections import Counter
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("analyze_open_results", Path(__file__).with_name("analyze-open-results.py"))
ANALYZER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ANALYZER)


def pair(request, outcome="success", code=None, stage=None, unknown=False, accepted=True, dispatched=True,
         started=True, phase="baseline"):
    """直接写契约样例，不调用被测汇总；Create evidence without the analyzer's aggregation code."""
    arrival = {"event": "arrival", "request": request, "phase": phase,
               "command": "PING" if request % 2 == 0 else "SET", "deadline_ns": 1000 + request * 100,
               "arrival_ns": 1010 + request * 100, "queued": 1, "accepted": accepted,
               "outcome": "enqueued" if accepted else "tester_dropped"}
    completion = {key: value for key, value in arrival.items() if key not in ("event", "accepted", "outcome")}
    completion.update(event="completion", dispatch_ns=1020 + request * 100 if dispatched else None,
                      completion_ns=1040 + request * 100, outcome=outcome, error_code=code, result_unknown=unknown)
    if started:
        completion["request_start_ns"] = 1030 + request * 100
    if stage:
        completion["stage"] = stage
    return arrival, completion


def fixture(directory, rows, planned=None):
    """保存独立 summary，故意允许其汇总遗漏逐 ID 错误；Allow aggregate-only summaries in fixtures."""
    directory.mkdir(parents=True, exist_ok=True)
    scheduled, counts, errors = Counter(), {}, {}
    for row in rows:
        if row["event"] not in ("arrival", "completion"):
            continue
        phase = row["phase"]
        counts.setdefault(phase, Counter())
        errors.setdefault(phase, Counter())
        if row["event"] == "arrival":
            scheduled[phase] += 1
            counts[phase]["scheduled"] += 1
            counts[phase][f"scheduled_{row['command']}"] += 1
            counts[phase]["enqueued" if row["accepted"] else "tester_dropped"] += 1
        else:
            counts[phase][row["outcome"]] += 1
            counts[phase]["dispatched"] += row.get("dispatch_ns") is not None
            counts[phase]["request_started"] += row.get("request_start_ns") is not None
            if row.get("error_code"):
                errors[phase][f"{row['outcome']}:{row['error_code']}"] += 1
    plan = {"phases": [{"name": name, "seconds": 1, "rate": count} for name, count in (planned or scheduled).items()]}
    summary = {"valid": True, "passed": True, "failure": None, "threads_alive": [],
               "recovery": {"recovered": True}, "phases": {name: {"counts": values, "errors": errors[name]}
                                                            for name, values in counts.items()}}
    for name, value in (("plan.json", plan), ("summary.json", summary)):
        (directory / name).write_text(json.dumps(value), encoding="utf-8")
    (directory / "requests.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
    return directory


class AnalyzeTests(unittest.TestCase):
    """身份核算与未知结果边界；Identity reconciliation and conservative unknown-result boundaries."""
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self):
        self.temporary.cleanup()

    def analyze(self, rows, planned=None):
        return ANALYZER.analyze_load(fixture(self.root / "load", rows, planned))

    def test_completion_can_precede_arrival(self):
        arrival, completion = pair(0)
        result = self.analyze([completion, arrival])
        self.assertTrue(result["evidence_consistent"], result["diagnostics"])
        self.assertEqual(result["phases"]["baseline"]["totals"]["success"], 1)

    def test_balanced_missing_and_duplicate_completions_fail_exact_ids(self):
        arrival0, completion0 = pair(0)
        arrival1, _ = pair(1)
        result = self.analyze([arrival0, completion0, arrival1, completion0])
        phase = result["phases"]["baseline"]
        self.assertTrue(all(phase["aggregate_accounting"].values()))
        self.assertEqual(phase["summary_count_differences"], {})
        self.assertFalse(result["evidence_consistent"])
        self.assertEqual(phase["identity_anomalies"]["duplicate_completion_ids"], 1)
        self.assertEqual(phase["identity_anomalies"]["accepted_without_exactly_one_completion_ids"], 2)

    def test_dropped_request_must_not_complete(self):
        arrival, completion = pair(0, accepted=False)
        result = self.analyze([arrival, completion])
        self.assertEqual(result["phases"]["baseline"]["identity_anomalies"]["dropped_with_completion_ids"], 1)
        self.assertFalse(result["evidence_consistent"])

    def test_unknown_set_results_are_not_success(self):
        cases = [
            pair(0, "client_transport_error", "LOCAL_EOF", "request"),
            pair(1, "client_timeout", "LOCAL_TIMEOUT", "request", unknown=True),
            pair(2),
            pair(3, "server_error", "ERR_COMMIT_UNKNOWN", "request", unknown=True),
            pair(4),
            pair(5, "server_error", "ERR_BUSY", "request"),
            pair(6),
            pair(7, "server_connect_error", "ERR_BUSY", "connect", started=False),
            pair(8),
            pair(9, "tester_cancelled", "LOCAL_DRAIN_DEADLINE", dispatched=False, started=False),
            pair(10),
            pair(11, "tester_cancelled", "LOCAL_DRAIN_DEADLINE", "request", unknown=True),
            pair(12),
            pair(13, "client_transport_error", "LOCAL_EOF", "request", unknown=True),
        ]
        result = self.analyze([row for case in cases for row in case])
        self.assertTrue(result["evidence_consistent"], result["diagnostics"])
        totals = result["phases"]["baseline"]["totals"]
        self.assertEqual(totals["success"], 6)
        self.assertEqual(totals["unknown_SET"], 4)
        self.assertEqual(totals["error_codes"]["LOCAL_EOF"], 2)
        self.assertEqual(totals["unknown_SET_by_error_code"]["LOCAL_EOF"], 1)
        self.assertEqual(totals["unknown_SET_by_stage"]["request"], 4)
        self.assertEqual(totals["queued_cancellations"], 1)

    def test_missing_unknown_flag_is_diagnostic(self):
        arrival, completion = pair(0)
        del completion["result_unknown"]
        result = self.analyze([arrival, completion])
        self.assertIn("missing_or_invalid_unknown_flag", result["diagnostics"])
        self.assertFalse(result["evidence_consistent"])

    def test_false_unknown_flag_is_diagnostic(self):
        case0 = pair(0)
        arrival, completion = pair(1, "client_timeout", "LOCAL_TIMEOUT", "request", unknown=False)
        result = self.analyze([*case0, arrival, completion])
        self.assertIn("unknown_flag_contract_mismatch", result["diagnostics"])

    def test_full_job_identity_is_compared(self):
        arrival, completion = pair(0)
        completion["queued"] += 1
        result = self.analyze([arrival, completion])
        self.assertEqual(result["phases"]["baseline"]["identity_anomalies"]["identity_mismatch_ids"], 1)

    def test_completion_without_arrival_and_duplicate_arrival(self):
        arrival, completion = pair(0)
        _, orphan = pair(1)
        result = self.analyze([arrival, arrival, completion, orphan])
        anomalies = result["phases"]["baseline"]["identity_anomalies"]
        self.assertEqual(anomalies["duplicate_arrival_ids"], 1)
        self.assertEqual(anomalies["completion_without_arrival_ids"], 1)

    def test_summary_mismatch_is_reported(self):
        rows = list(pair(0))
        directory = fixture(self.root / "load", rows)
        path = directory / "summary.json"
        summary = json.loads(path.read_text())
        summary["phases"]["baseline"]["counts"]["success"] = 99
        path.write_text(json.dumps(summary), encoding="utf-8")
        result = ANALYZER.analyze_load(directory)
        self.assertEqual(result["phases"]["baseline"]["summary_count_differences"]["success"],
                         {"raw": 1, "summary": 99})

    def test_incomplete_json_is_reported(self):
        directory = fixture(self.root / "load", pair(0))
        with (directory / "requests.jsonl").open("a", encoding="utf-8") as stream:
            stream.write('{"event":')
        result = ANALYZER.analyze_load(directory)
        self.assertIn("malformed_raw_json", result["diagnostics"])

    def test_missing_final_newline_is_reported(self):
        directory = fixture(self.root / "load", pair(0))
        path = directory / "requests.jsonl"
        path.write_bytes(path.read_bytes().rstrip(b"\n"))
        result = ANALYZER.analyze_load(directory)
        self.assertIn("raw_missing_final_newline", result["diagnostics"])

    def test_non_string_terminal_is_diagnostic(self):
        arrival, completion = pair(0)
        completion["outcome"] = []
        directory = fixture(self.root / "load", [arrival])
        with (directory / "requests.jsonl").open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(completion) + "\n")
        result = ANALYZER.analyze_load(directory)
        self.assertIn("unknown_terminal_outcome", result["diagnostics"])

    def test_missing_final_summary_is_reported(self):
        directory = fixture(self.root / "load", pair(0))
        (directory / "summary.json").unlink()
        result = ANALYZER.analyze_load(directory)
        self.assertIn("metadata_unavailable", result["diagnostics"])

    def test_plan_not_just_summary_controls_scheduled_count(self):
        result = self.analyze(pair(0), {"baseline": 2})
        self.assertIn("planned_scheduled_mismatch", result["diagnostics"])

    def test_recovery_and_connection_events_are_separate(self):
        result = self.analyze([*pair(0), {"event": "recovery_ping", "outcome": "success"},
                               {"event": "connection_replaced", "worker": 0}])
        self.assertTrue(result["evidence_consistent"], result["diagnostics"])
        self.assertEqual(result["phases"]["baseline"]["totals"]["completion_records"], 1)
        self.assertEqual(result["events"]["recovery_ping"], 1)

    def test_mode_repeat_and_phase_survive_root_discovery(self):
        (self.root / "plan.json").write_text(json.dumps({"repeats": 1}), encoding="utf-8")
        (self.root / "summary.json").write_text(json.dumps({"passed": True}), encoding="utf-8")
        for mode in ("fast", "reliable"):
            directory = fixture(self.root / f"{mode}-0" / "load", [*pair(0), *pair(1, phase="recovery")])
            (directory.parent / "result.json").write_text(json.dumps({"mode": mode, "repeat": 0}), encoding="utf-8")
        result = ANALYZER.analyze_root(self.root)
        self.assertTrue(result["evidence_consistent"], result["diagnostics"])
        self.assertEqual([(row["mode"], row["repeat"]) for row in result["rounds"]], [("fast", 0), ("reliable", 0)])
        self.assertEqual(result["rounds"][0]["phases"]["recovery"]["totals"]["scheduled"], 1)

    def test_partial_root_cannot_be_consistent(self):
        (self.root / "plan.json").write_text(json.dumps({"repeats": 1}), encoding="utf-8")
        fixture(self.root / "fast-0" / "load", pair(0))
        result = ANALYZER.analyze_root(self.root)
        self.assertFalse(result["evidence_consistent"])
        self.assertIn("unfinished_round", result["diagnostics"])


if __name__ == "__main__":
    unittest.main()
