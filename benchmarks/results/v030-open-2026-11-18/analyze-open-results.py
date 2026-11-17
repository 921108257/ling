#!/usr/bin/env python3
"""离线核算开放负载原始请求；Independently reconcile completed open-load evidence.

Only reads evidence. A temporary SQLite index bounds identity-check memory; no load is run.
只读原始证据，临时 SQLite 保存请求身份；不启动负载，不把结果未知计为成功。
"""
import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import re
import sqlite3
import tempfile

TERMINALS = ("success", "server_error", "server_connect_error", "client_timeout",
             "client_protocol_error", "client_transport_error", "tester_error", "tester_cancelled")
JOB_FIELDS = ("phase", "command", "deadline_ns", "arrival_ns", "queued")
SCALARS = ("scheduled", "accepted", "dropped", "completion_records", "success", "dispatched",
           "request_started", "unknown_SET", "queued_cancellations")
MAPS = ("outcomes", "error_codes", "outcome_error_codes", "unknown_SET_by_outcome",
        "unknown_SET_by_error_code", "unknown_SET_by_stage")
ANOMALIES = {
    "duplicate_arrival_ids": "arrivals > 1",
    "duplicate_completion_ids": "completions > 1",
    "completion_without_arrival_ids": "arrivals = 0 AND completions > 0",
    "accepted_without_exactly_one_completion_ids": "arrivals > 0 AND accepted = 1 AND completions != 1",
    "dropped_with_completion_ids": "arrivals > 0 AND accepted = 0 AND completions != 0",
    "invalid_acceptance_ids": "arrivals > 0 AND accepted IS NULL",
    "identity_mismatch_ids": "identities_differ > 0",
    "acceptance_mismatch_ids": "acceptance_differ > 0",
}


class Diagnostics:
    """诊断仅保留有限例子；Retain counts and bounded examples, not every invalid row."""
    def __init__(self):
        self.counts, self.examples = Counter(), defaultdict(list)

    def add(self, code, detail):
        self.counts[code] += 1
        if len(self.examples[code]) < 5:
            self.examples[code].append(detail)

    def result(self):
        return {code: {"count": count, "examples": self.examples[code]}
                for code, count in sorted(self.counts.items())}


def integer(value):
    """排除 JSON bool；JSON booleans are not request IDs or clock values."""
    return type(value) is int and 0 <= value < 2 ** 63


def new_bucket():
    """独立保留成功与未知计数；Keep successful and unknown results separate."""
    return {**dict.fromkeys(SCALARS, 0), **{name: Counter() for name in MAPS}}


def json_file(path, diagnostics, required=True):
    """缺少或损坏的收尾文件不能默认为成功；Missing/faulty final files remain explicit."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(value, dict):
            raise ValueError("expected an object")
        return value
    except (OSError, ValueError) as error:
        if required or path.exists():
            diagnostics.add("metadata_unavailable", {"path": str(path), "error": str(error)})
        return {}


def expected_count(phase):
    """复现调度的整数纳秒边界，不借用 summary；Reproduce plan boundaries independently."""
    seconds, rate = phase["seconds"], phase["rate"]
    if (type(seconds) not in (int, float) or type(rate) not in (int, float)
            or not math.isfinite(seconds) or not math.isfinite(rate)
            or seconds <= 0 or rate <= 0 or seconds * rate > 2_000_000):
        raise ValueError("invalid or out-of-scope phase plan")
    duration, count = round(seconds * 1e9), math.ceil(seconds * rate)
    while count and round((count - 1) * 1e9 / rate) >= duration:
        count -= 1
    while round(count * 1e9 / rate) < duration:
        count += 1
    return count


def validate_completion(row, line, diagnostics):
    """依据冻结工具的分类契约复核 unknown；Recheck the tool's conservative unknown contract."""
    outcome, code, stage = row.get("outcome"), row.get("error_code"), row.get("stage")
    command, started = row.get("command"), row.get("request_start_ns")
    if outcome not in TERMINALS:
        diagnostics.add("unknown_terminal_outcome", {"line": line, "outcome": outcome})
        return
    if type(row.get("result_unknown")) is not bool:
        diagnostics.add("missing_or_invalid_unknown_flag", {"line": line})
    if code is not None and not isinstance(code, str):
        diagnostics.add("invalid_error_code", {"line": line})
    expected_unknown = False
    if outcome == "success":
        if code is not None or row.get("dispatch_ns") is None or started is None:
            diagnostics.add("invalid_success_fields", {"line": line})
    elif outcome == "tester_cancelled":
        expected_unknown = command == "SET" and started is not None
        if code != "LOCAL_DRAIN_DEADLINE":
            diagnostics.add("invalid_cancellation_code", {"line": line})
    else:
        if stage not in ("connect", "request"):
            diagnostics.add("invalid_error_stage", {"line": line, "stage": stage})
        if (stage == "request" and started is None) or (stage == "connect" and started is not None):
            diagnostics.add("stage_request_start_mismatch", {"line": line, "stage": stage})
        expected_unknown = command == "SET" and stage == "request" and (
            outcome != "server_error" or code == "ERR_COMMIT_UNKNOWN")
        expected_codes = {"client_timeout": ("LOCAL_TIMEOUT",), "tester_error": ("LOCAL_TESTER",),
                          "client_protocol_error": ("LOCAL_PROTOCOL", "LOCAL_ENCODING", "LOCAL_RESPONSE")}
        if outcome in expected_codes and code not in expected_codes[outcome]:
            diagnostics.add("outcome_code_mismatch", {"line": line, "outcome": outcome, "code": code})
        if outcome in ("server_error", "server_connect_error"):
            expected_stage = "request" if outcome == "server_error" else "connect"
            if stage != expected_stage or not isinstance(code, str) or not re.fullmatch(r"ERR_[A-Z0-9_]+", code):
                diagnostics.add("server_error_contract_mismatch", {"line": line})
        if outcome == "client_transport_error" and (not isinstance(code, str) or not code.startswith("LOCAL_")):
            diagnostics.add("transport_error_contract_mismatch", {"line": line, "code": code})
    if type(row.get("result_unknown")) is bool and row["result_unknown"] != expected_unknown:
        diagnostics.add("unknown_flag_contract_mismatch", {"line": line, "expected": expected_unknown})
    clocks = [row.get("arrival_ns")]
    for name in ("dispatch_ns", "request_start_ns", "completion_ns"):
        value = row.get(name)
        if value is not None:
            if not integer(value):
                diagnostics.add("invalid_completion_clock", {"line": line, "field": name})
            clocks.append(value)
    if row.get("completion_ns") is None:
        diagnostics.add("missing_completion_clock", {"line": line})
    if all(integer(value) for value in clocks) and clocks != sorted(clocks):
        diagnostics.add("completion_clock_order", {"line": line})
    if row.get("dispatch_ns") is None and (outcome != "tester_cancelled" or started is not None):
        diagnostics.add("invalid_undispatched_completion", {"line": line})


def update_bucket(bucket, row):
    """原始记录计数，重复记录也保留供核算；Count raw records, including duplicates."""
    if row["event"] == "arrival":
        bucket["scheduled"] += 1
        if row.get("accepted") is True:
            bucket["accepted"] += 1
        elif row.get("accepted") is False:
            bucket["dropped"] += 1
        return
    outcome, code = row.get("outcome"), row.get("error_code")
    bucket["completion_records"] += 1
    bucket["outcomes"][str(outcome)] += 1
    bucket["success"] += outcome == "success"
    bucket["dispatched"] += row.get("dispatch_ns") is not None
    bucket["request_started"] += row.get("request_start_ns") is not None
    bucket["queued_cancellations"] += outcome == "tester_cancelled" and row.get("dispatch_ns") is None
    if code:
        bucket["error_codes"][str(code)] += 1
        bucket["outcome_error_codes"][f"{outcome}:{code}"] += 1
    if row.get("result_unknown") is True and row.get("command") == "SET":
        bucket["unknown_SET"] += 1
        bucket["unknown_SET_by_outcome"][str(outcome)] += 1
        bucket["unknown_SET_by_error_code"][str(code)] += 1
        bucket["unknown_SET_by_stage"][str(row.get("stage", "absent"))] += 1


def accounting(bucket):
    """同时按阶段和命令公开汇总等式；Expose aggregate equalities per phase and command."""
    return {"scheduled_equals_accepted_plus_dropped": bucket["scheduled"] == bucket["accepted"] + bucket["dropped"],
            "accepted_equals_terminal_records": bucket["accepted"] == bucket["completion_records"]}


def analyze_load(directory, mode=None, repeat=None):
    """逐行扫描与磁盘身份索引；Stream raw rows and reconcile each request on disk."""
    directory = Path(directory)
    diagnostics, buckets, events = Diagnostics(), defaultdict(new_bucket), Counter()
    plan = json_file(directory / "plan.json", diagnostics)
    summary = json_file(directory / "summary.json", diagnostics)
    parent_result = json_file(directory.parent / "result.json", diagnostics, required=False)
    for name, expected in (("mode", mode), ("repeat", repeat)):
        if expected is not None and parent_result.get(name, expected) != expected:
            diagnostics.add("round_identity_mismatch", {"field": name, "expected": expected,
                                                       "actual": parent_result.get(name)})
    mode, repeat = parent_result.get("mode", mode), parent_result.get("repeat", repeat)
    candidates = [directory / name for name in ("requests.jsonl", "events.jsonl")]
    raw = next((candidate for candidate in candidates if candidate.is_file()), candidates[0])
    digest, total_lines, indexed_records = hashlib.sha256(), 0, 0
    identity_checks, identity_examples, sequence = {}, {}, {}
    try:
        before = raw.stat()
        with tempfile.TemporaryDirectory(prefix="ling-open-analysis-") as temporary:
            database = sqlite3.connect(str(Path(temporary) / "identities.sqlite"))
            try:
                database.execute("PRAGMA journal_mode=OFF")
                database.execute("PRAGMA synchronous=OFF")
                database.execute("PRAGMA cache_size=-8192")
                database.execute("""CREATE TABLE jobs(request INTEGER PRIMARY KEY, signature TEXT, phase TEXT,
                    command TEXT, arrivals INTEGER, completions INTEGER, accepted INTEGER,
                    identities_differ INTEGER, acceptance_differ INTEGER)""")
                insert = """INSERT INTO jobs VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0)
                    ON CONFLICT(request) DO UPDATE SET
                    arrivals=jobs.arrivals+excluded.arrivals, completions=jobs.completions+excluded.completions,
                    identities_differ=jobs.identities_differ+(jobs.signature != excluded.signature),
                    acceptance_differ=jobs.acceptance_differ+CASE WHEN jobs.accepted IS NOT NULL
                        AND excluded.accepted IS NOT NULL AND jobs.accepted != excluded.accepted THEN 1 ELSE 0 END,
                    accepted=COALESCE(jobs.accepted, excluded.accepted)"""
                last_newline = True
                with raw.open("rb") as stream:
                    for total_lines, data in enumerate(stream, 1):
                        last_newline = data.endswith(b"\n")
                        digest.update(data)
                        try:
                            row = json.loads(data)
                            if not isinstance(row, dict):
                                raise ValueError("expected an object")
                        except (ValueError, UnicodeError) as error:
                            diagnostics.add("malformed_raw_json", {"line": total_lines, "error": str(error)})
                            continue
                        event = row.get("event")
                        events[str(event)] += 1
                        if event not in ("arrival", "completion"):
                            if event == "worker_failure":
                                diagnostics.add("worker_failure_event", {"line": total_lines})
                            continue
                        phase, command, request = row.get("phase"), row.get("command"), row.get("request")
                        if not isinstance(phase, str) or command not in ("PING", "SET"):
                            diagnostics.add("invalid_job_scope", {"line": total_lines})
                            continue
                        for scope in ((phase, "all"), (phase, command)):
                            update_bucket(buckets[scope], row)
                        if (not integer(request) or any(not integer(row.get(name))
                                for name in ("deadline_ns", "arrival_ns", "queued"))):
                            diagnostics.add("invalid_job_identity", {"line": total_lines})
                            continue
                        if row["arrival_ns"] < row["deadline_ns"]:
                            diagnostics.add("arrival_before_deadline", {"line": total_lines})
                        if command != ("PING" if request % 2 == 0 else "SET"):
                            diagnostics.add("request_command_parity", {"line": total_lines, "request": request})
                        accepted = None
                        if event == "arrival":
                            if type(row.get("accepted")) is bool:
                                accepted = int(row["accepted"])
                            expected = "enqueued" if accepted == 1 else "tester_dropped"
                            if accepted is None or row.get("outcome") != expected:
                                diagnostics.add("invalid_arrival_acceptance", {"line": total_lines})
                        else:
                            validate_completion(row, total_lines, diagnostics)
                        signature = json.dumps([row.get(name) for name in JOB_FIELDS], separators=(",", ":"))
                        database.execute(insert, (request, signature, phase, command, int(event == "arrival"),
                                                  int(event == "completion"), accepted))
                        indexed_records += 1
                if not last_newline:
                    diagnostics.add("raw_missing_final_newline", {"line": total_lines})
                database.commit()
                terms = ", ".join(f"SUM(CASE WHEN {condition} THEN 1 ELSE 0 END)" for condition in ANOMALIES.values())
                for phase, command, *counts in database.execute(f"SELECT phase, command, {terms} FROM jobs GROUP BY phase, command"):
                    for scope in ((phase, command), (phase, "all")):
                        row = identity_checks.setdefault(scope, dict.fromkeys(ANOMALIES, 0))
                        for name, count in zip(ANOMALIES, counts):
                            row[name] += count
                for name, condition in ANOMALIES.items():
                    count = database.execute(f"SELECT COUNT(*) FROM jobs WHERE {condition}").fetchone()[0]
                    if count:
                        diagnostics.add("request_identity_accounting", {"kind": name, "request_ids": count})
                        identity_examples[name] = [row[0] for row in database.execute(
                            f"SELECT request FROM jobs WHERE {condition} ORDER BY request LIMIT 5")]
                distinct, minimum, maximum = database.execute(
                    "SELECT COUNT(*), MIN(request), MAX(request) FROM jobs WHERE arrivals > 0").fetchone()
                sequence = {"distinct_arrival_ids": distinct, "minimum": minimum, "maximum": maximum,
                            "contiguous_from_zero": distinct == 0 or (minimum == 0 and maximum == distinct - 1)}
                if not sequence["contiguous_from_zero"]:
                    diagnostics.add("noncontiguous_request_ids", sequence)
            finally:
                database.close()
        after = raw.stat()
        stable = (before.st_size, before.st_mtime_ns) == (after.st_size, after.st_mtime_ns)
        if not stable:
            diagnostics.add("raw_changed_during_analysis", {"path": str(raw)})
    except OSError as error:
        before, stable = None, False
        diagnostics.add("raw_unavailable", {"path": str(raw), "error": str(error)})
    planned = {}
    try:
        for phase in plan["phases"]:
            name = phase["name"]
            if name in planned:
                raise ValueError("duplicate planned phase")
            planned[name] = expected_count(phase)
    except (KeyError, TypeError, ValueError, OverflowError) as error:
        diagnostics.add("invalid_phase_plan", {"error": str(error)})
    phases = {}
    reported_phases = summary.get("phases", {})
    if not isinstance(reported_phases, dict):
        diagnostics.add("invalid_summary_phases", {})
        reported_phases = {}
    for phase in sorted(set(planned) | {name for name, _ in buckets} | set(reported_phases)):
        total = buckets[(phase, "all")]
        actual_counts = {"scheduled": total["scheduled"], "enqueued": total["accepted"],
                         "tester_dropped": total["dropped"], "dispatched": total["dispatched"],
                         "request_started": total["request_started"], **total["outcomes"]}
        for command in ("PING", "SET"):
            actual_counts[f"scheduled_{command}"] = buckets[(phase, command)]["scheduled"]
        reported = reported_phases.get(phase, {})
        counts = reported.get("counts", {}) if isinstance(reported, dict) else {}
        errors = reported.get("errors", {}) if isinstance(reported, dict) else {}
        if not isinstance(counts, dict) or not isinstance(errors, dict):
            diagnostics.add("invalid_summary_counts", {"phase": phase})
            counts, errors = {}, {}
        differences = {key: {"raw": actual_counts.get(key, 0), "summary": counts.get(key, 0)}
                       for key in sorted(set(actual_counts) | set(counts))
                       if actual_counts.get(key, 0) != counts.get(key, 0)}
        error_differences = {key: {"raw": total["outcome_error_codes"].get(key, 0), "summary": errors.get(key, 0)}
                             for key in sorted(set(total["outcome_error_codes"]) | set(errors))
                             if total["outcome_error_codes"].get(key, 0) != errors.get(key, 0)}
        if differences or error_differences:
            diagnostics.add("summary_raw_mismatch", {"phase": phase})
        if phase not in planned or total["scheduled"] != planned[phase]:
            diagnostics.add("planned_scheduled_mismatch", {"phase": phase, "raw": total["scheduled"],
                                                           "planned": planned.get(phase)})
        aggregate = accounting(total)
        if not all(aggregate.values()):
            diagnostics.add("aggregate_accounting_mismatch", {"phase": phase})
        phases[phase] = {"planned_scheduled": planned.get(phase), "totals": total,
                         "by_command": {command: {"counts": buckets[(phase, command)],
                            "aggregate_accounting": accounting(buckets[(phase, command)]),
                            "identity_anomalies": identity_checks.get((phase, command), dict.fromkeys(ANOMALIES, 0))}
                                        for command in ("PING", "SET")},
                         "aggregate_accounting": aggregate,
                         "identity_anomalies": identity_checks.get((phase, "all"), dict.fromkeys(ANOMALIES, 0)),
                         "summary_count_differences": differences, "summary_error_differences": error_differences}
    return {"mode": mode, "repeat": repeat, "load_directory": str(directory),
            "raw": {"path": str(raw), "sha256": digest.hexdigest() if before else None,
                    "bytes": before.st_size if before else None, "lines": total_lines,
                    "indexed_records": indexed_records, "stable_during_read": stable},
            "input_summary_flags": {name: summary.get(name) for name in ("valid", "passed", "failure", "threads_alive", "recovery")},
            "input_process_flags": {name: parent_result.get(name) for name in ("passed", "failure", "driver_exit", "cleanup_errors")},
            "events": dict(events), "request_sequence": sequence, "identity_examples": identity_examples,
            "phases": phases, "evidence_consistent": not diagnostics.counts, "diagnostics": diagnostics.result()}


def analyze_root(root):
    """只发现约定目录，不递归误混其他轮次；Discover only the known result layout."""
    root, diagnostics = Path(root), Diagnostics()
    if any((root / name).exists() for name in ("requests.jsonl", "events.jsonl")):
        loads = [(root, None, None)]
    elif (root / "load").is_dir():
        loads = [(root / "load", None, None)]
    else:
        plan = json_file(root / "plan.json", diagnostics)
        final = json_file(root / "summary.json", diagnostics)
        loads = []
        repeats = plan.get("repeats")
        if type(repeats) is not int or not 1 <= repeats <= 3:
            diagnostics.add("invalid_root_repeats", {"repeats": repeats})
            repeats = 0
        for mode in ("fast", "reliable"):
            for repeat in range(repeats):
                directory = root / f"{mode}-{repeat}"
                if not (directory / "result.json").is_file():
                    diagnostics.add("unfinished_round", {"path": str(directory)})
                loads.append((directory / "load", mode, repeat))
        if not final:
            diagnostics.add("unfinished_root", {"path": str(root)})
    rounds = [analyze_load(directory, mode, repeat) for directory, mode, repeat in loads]
    return {"schema_version": 1, "root": str(root),
            "analyzer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "semantics": ["Counts are raw records assigned to each request's scheduled phase; no throughput rate is inferred.",
                          "Successful completions and result_unknown SET records are separate; unknown does not imply committed or lost.",
                          "request_start_ns means the request call began, not proof that socket transmission completed.",
                          "Per-request pairing permits completion before arrival in file order.",
                          "Evidence consistency is independent of product acceptance and original valid/passed flags.",
                          "Temporary SQLite stores request identities; malformed rows and missing final files invalidate consistency."],
            "evidence_consistent": bool(rounds) and not diagnostics.counts and all(row["evidence_consistent"] for row in rounds),
            "diagnostics": diagnostics.result(), "rounds": rounds}


def main(argv=None):
    """不覆盖原始证据，仅输出新的分析 JSON；Emit independent JSON, never modify raw evidence."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(argv)
    result = analyze_root(args.root)
    rendered = json.dumps(result, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        with args.output.open("x", encoding="utf-8") as stream:
            stream.write(rendered)
        print(json.dumps({"output": str(args.output), "evidence_consistent": result["evidence_consistent"]}))
    else:
        print(rendered, end="")
    return 0 if result["evidence_consistent"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
