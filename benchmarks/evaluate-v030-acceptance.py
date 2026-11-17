#!/usr/bin/env python3
"""按已冻结计划核对 0.3 正式长测；不启动负载、不修改原件。
Evaluate formal 0.3 evidence against a frozen plan without running load or editing inputs.
"""
import argparse
from collections import Counter
from fractions import Fraction
import gzip
import hashlib
import importlib.util
import json
import math
from pathlib import Path, PurePosixPath
import sys
import tempfile
import zlib

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parent.parent
ANALYZER = REPO / "benchmarks/v030_analyze.py"
GROUPS = tuple(f"{speed}-db{db}" for db in range(2) for speed in ("fast", "slow"))
POLICIES = ("noeviction", "lru", "lfu", "random", "ttl-first")
INSTANCES = {f"{mode}-{phase}" for mode in ("fast", "reliable") for phase in ("shadow", "pressure")}
EPSILON = 1e-6


def require(condition, explanation):
    """证据不足也判失败；Insufficient evidence fails just like a violated limit."""
    if not condition:
        raise ValueError(explanation)


def number(value, nonnegative=True):
    """拒绝 bool、NaN、Infinity 和缺失；Reject bools, nonfinite values, and missing fields."""
    require(type(value) in (int, float) and math.isfinite(value), "finite JSON number required")
    require(not nonnegative or value >= 0, "nonnegative number required")
    return value


def integer(value):
    """计数不经过浮点；Counts never pass through floating point."""
    if type(value) is str and value.isascii() and value.isdecimal() and len(value) <= 40:
        value = int(value)
    require(type(value) is int and value >= 0, "nonnegative integer required")
    return value


def pairs_unique(pairs):
    """重复 JSON 字段不能覆盖证据；Duplicate JSON keys cannot replace evidence."""
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON key: " + key)
        result[key] = value
    return result


def read_json(path):
    """严格读取元数据；Read strict JSON metadata."""
    def invalid(value):
        raise ValueError("nonfinite JSON token: " + value)
    with Path(path).open(encoding="utf-8") as stream:
        return json.load(stream, object_pairs_hook=pairs_unique, parse_constant=invalid)


def sha256(path):
    """流式计算原始字节摘要；Hash original bytes incrementally."""
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def relative(root, value):
    """只接受根目录内的相对路径；Accept only relative paths contained by the evidence root."""
    require(isinstance(value, str) and value and "\\" not in value, "normalized relative path required")
    path = Path(value)
    require(not path.is_absolute() and ".." not in path.parts, "relative path escapes root")
    resolved = (root / path).resolve()
    require(resolved.is_relative_to(root), "resolved path escapes root")
    return resolved


def analyzer_module():
    """复用已存在的分析器；Reuse the existing analyzer rather than reimplementing it."""
    spec = importlib.util.spec_from_file_location("v030_acceptance_base", ANALYZER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Checks:
    """独立记录各项失败，缺字段不会变成零；Record independent failures without replacing missing fields by zero."""
    def __init__(self):
        self.rows = []

    def run(self, name, evidence, operation):
        """失败保留原因，继续其他独立项；Retain failures and continue independent checks."""
        try:
            observed = operation()
            self.rows.append({"check": name, "passed": True, "evidence": evidence, "observed": observed})
        except (ValueError, KeyError, TypeError, IndexError, OSError, EOFError, OverflowError, zlib.error) as error:
            self.rows.append({"check": name, "passed": False, "evidence": evidence,
                              "error": type(error).__name__ + ": " + str(error)})


def metric_sets(workers):
    """固定必需计数，不允许计划漏掉慢组；Fix required metrics, including every slow consumer."""
    totals = [f"worker_{i}_cycles" for i in range(workers)] + ["producer_write_confirmations"]
    totals += ["ack_unique." + group for group in GROUPS]
    return totals, totals + ["committed"]


def capacity_thresholds(phase):
    """ttl-first可用淘汰或OOM证明触限；TTL-first may prove capacity with eviction or OOM."""
    oom, evicted = (integer(phase["capacity"][key]) for key in ("minimum_oom", "minimum_evicted"))
    if phase["phase"] == "pressure":
        threshold = max(oom, evicted) if phase["policy"] == "ttl-first" else oom if phase["policy"] == "noeviction" else evicted
        require(threshold > 0, "pressure policy needs actual capacity evidence")
    return oom, evicted


def validate_calibration(plan, phase):
    """固定校准基数与0.8换算，零观测不能形成零门槛；Check fixed calibration bases and the 0.8 derivation."""
    calibration = plan["calibration"]
    require(number(calibration["factor"]) == .8, "calibration factor differs")
    name = Path(phase["directory"]).name
    baseline = calibration["phases"][name]
    elapsed = Fraction(str(number(baseline["actual_load_seconds"])))
    require(elapsed > 0, "calibration load duration must be positive")
    for key, source in (("minimum_totals", "total_deltas"), ("minimum_window", "minimum_window")):
        require(set(phase[key]) == set(baseline[source]), "calibration metric set differs")
        for metric, threshold in phase[key].items():
            value = integer(baseline[source][metric])
            require(value > 0, "zero calibration baseline: " + metric)
            scaled = value * Fraction(4, 5)
            if key == "minimum_totals":
                scaled = scaled / elapsed * Fraction(str(phase["requested_seconds"]))
            require(integer(threshold) == math.ceil(scaled), "threshold differs from calibration: " + metric)
    require(set(baseline["minimum_window_active_seconds"]) == set(GROUPS), "four active calibration baselines required")
    for group, threshold in phase["minimum_window_active_seconds"].items():
        value = number(baseline["minimum_window_active_seconds"][group])
        require(value > 0 and math.isclose(threshold, value * .8, rel_tol=0, abs_tol=EPSILON), "active threshold differs from calibration")


def validate_plan(plan, root):
    """计划必须完整并保持正式布局，空占位不能验收；Require complete thresholds and the formal layout."""
    require(plan["format"] == "ling-v030-acceptance-plan-1", "unknown plan format")
    source = PurePosixPath(plan["source_root"])
    require(source.is_absolute() and ".." not in source.parts and str(source) == plan["source_root"], "normalized WSL source root required")
    require(source.name == plan["run_id"] == root.name, "run identity differs")
    require(set(plan["instances"]) == INSTANCES, "four exact instances required")
    require(set(plan["analysis_directories"]) == INSTANCES and len(plan["analysis_directories"]) == 4,
            "four unique analyzer input roots required")
    require(plan["analysis_workers"] is None or type(plan["analysis_workers"]) is int, "invalid analyzer override")
    require(sha256(ANALYZER) == plan["analyzer_sha256"], "analyzer differs from frozen plan")
    limits = plan["limits"]
    require(limits["window_seconds"] == 60 and limits["warmup_seconds"] == 3600, "formal timing differs")
    for key in ("max_sample_start_gap_seconds", "max_sample_duration_seconds"):
        require(number(limits[key]) > 0, "sample limits must be positive")
    for key, maximum in (("rss_bytes_per_hour", 1048576), ("handles_per_hour", .1), ("threads_per_hour", .1)):
        require(0 < number(limits[key]) <= maximum, "resource slope limit weakened")
    expected = {f"{mode}-{phase}-{policy}" for mode in ("fast", "reliable") for phase in ("shadow", "pressure")
                for policy in (("noeviction",) if phase == "shadow" else POLICIES)}
    require(len(plan["phases"]) == 12, "twelve planned phases required")
    sources = plan["calibration"]["source_sha256"]
    require(isinstance(sources, dict) and sources, "calibration sources required")
    for path, digest in sources.items():
        require(sha256(relative(REPO, path)) == digest, "calibration source hash differs: " + path)
    names = set()
    for phase in plan["phases"]:
        name = f"{phase['event_mode']}-{phase['phase']}-{phase['policy']}"
        require(name not in names, "duplicate planned phase")
        names.add(name)
        instance = f"{phase['event_mode']}-{phase['phase']}"
        require(phase["directory"] == instance + "/" + name, "phase directory differs from identity")
        relative(root, phase["directory"])
        duration = 259500 if phase["phase"] == "shadow" else 51840
        require(number(phase["requested_seconds"]) == duration, "formal duration differs")
        settings = plan["instances"][instance]["settings"]
        workers = settings["workers"]
        require(type(workers) is int and workers == 2, "formal plan requires two workers")
        require(settings["warmup_seconds"] == 3600 and settings["seconds"] == duration, "instance timing differs")
        require(plan["analysis_workers"] in (None, workers), "analyzer override differs from workers")
        for key, metrics in zip(("minimum_totals", "minimum_window"), metric_sets(workers)):
            require(set(phase[key]) == set(metrics), "missing/extra mandatory threshold")
            require(all(integer(value) > 0 for value in phase[key].values()), "zero or missing workload threshold")
        require(set(phase["minimum_window_active_seconds"]) == set(GROUPS), "four active thresholds required")
        require(all(0 < number(value) <= 60 for value in phase["minimum_window_active_seconds"].values()), "invalid active threshold")
        require(set(phase["offline"]) == set(GROUPS), "four offline budgets required")
        for group, budget in phase["offline"].items():
            require(0 <= integer(budget["min_reconnects"]) <= integer(budget["max_reconnects"]), "invalid reconnect range")
            require(0 <= number(budget["min_seconds"]) <= number(budget["max_seconds"]), "invalid offline duration range")
            if group.startswith("fast-"):
                require(all(value == 0 for value in budget.values()), "fast consumers must stay online")
            else:
                require(budget["min_reconnects"] > 0 and budget["min_seconds"] > 0, "slow offline proof required")
        capacity_thresholds(phase)
        validate_calibration(plan, phase)
    require(names == expected, "planned phase set differs")
    require(set(plan["calibration"]["phases"]) == names, "calibration phase set differs")
    return {"phases": len(names), "instances": sorted(INSTANCES)}


def source_inventory(root, plan, plan_path, analysis_path):
    """列出实际读取的原件，不声称审计未读取的 Journal；Inventory consumed sources, not unread journal contents."""
    paths = {plan_path.resolve(), analysis_path.resolve(), ANALYZER.resolve(), Path(__file__).resolve(),
             root / "layout.json", root / "layout-result.json"}
    paths.update(root / "tool_source" / name for name in ("run-v030-layout.py", "v030.py", "v020.py"))
    paths.update(relative(REPO, path) for path in plan["calibration"]["source_sha256"])
    for name in INSTANCES:
        paths.update((root / name / "environment.json", root / name / "tool_source/v030.py",
                      root / name / "tool_source/v020.py", root / name / "ling"))
    for phase in plan["phases"]:
        directory = relative(root, phase["directory"])
        paths.update(directory / name for name in ("result.json", "ling.toml", "resources.jsonl.gz",
                     "errors.jsonl.gz", "consumer-events.jsonl.gz", "server-complete.log.gz"))
    return {str(path): {"sha256": sha256(path), "bytes": path.stat().st_size} for path in sorted(paths)}


def bind_analysis(base, root, plan, analysis_path):
    """同路径、同参数复算并逐字节比对，不能由旧报告冒充当前原件。
    Recompute at the same paths/arguments and compare bytes, preventing stale-report substitution.
    """
    with tempfile.TemporaryDirectory(prefix="ling-v030-evaluate-") as temporary:
        output = Path(temporary) / "analysis.json"
        arguments = [str(relative(root, name)) for name in plan["analysis_directories"]]
        arguments += ["--output", str(output)]
        if plan["analysis_workers"] is not None:
            arguments += ["--workers", str(plan["analysis_workers"])]
        require(base.main(arguments) == 0, "analyzer failed")
        observed = sha256(output)
        require(observed == sha256(analysis_path), "report differs from recomputed current evidence")
    return {"sha256": observed, "input_directories": plan["analysis_directories"], "workers": plan["analysis_workers"]}


def validate_source_paths(layout, plan, root):
    """完整源路径绑定运行身份，不只比较目录末段；Bind run identity using complete recorded source paths."""
    commands = layout["commands"]
    require(len(commands) == 4 and {row["name"] for row in commands} == INSTANCES, "four recorded commands required")
    source = PurePosixPath(plan["source_root"])
    for row in commands:
        command, expected = row["command"], str(source / row["name"])
        require(isinstance(command, list) and command.count(expected) == 1, "recorded output path differs")
        index = command.index(expected)
        require((index > 0 and command[index - 1] == "--output") or command[index + 1:index + 2] == ["--event-mode"], "unrecognized output argument")
        require(command[command.index("--binary-revision") + 1] == plan["binary"]["revision"], "command revision differs")
    require(layout["binary_sha256"] == plan["binary"]["sha256"] and layout["binary_revision"] == plan["binary"]["revision"], "layout binary identity differs")
    recorded = {}
    for path, digest in layout["script_and_tool_sha256"].items():
        name = PurePosixPath(path).name
        require(name not in recorded, "ambiguous recorded tool basename")
        recorded[name] = digest
    require(recorded["run-v030-layout.py"] == plan["layout_sha256"] == sha256(root / "tool_source/run-v030-layout.py"), "layout tool bytes differ")
    for name in INSTANCES:
        frozen = plan["instances"][name]
        require(recorded["v030.py"] == frozen["script_sha256"] == sha256(root / "tool_source/v030.py"), "root driver bytes differ")
        require(recorded["v020.py"] == frozen["v020_sha256"] == sha256(root / "tool_source/v020.py"), "root wire dependency bytes differ")
    return {"source_root": str(source), "run_id": plan["run_id"]}


def validate_layout(layout, phases):
    """重算真实共同覆盖，同时要求编排器成功；Recompute actual overlap and require clean orchestration."""
    require(layout["orchestration_passed"] is True and layout["errors"] == [], "orchestration failed")
    require(layout["cleanup_events"] == [] and layout["remaining_processes"] == [], "cleanup or residual processes")
    exits = layout["driver_exits"]
    require(len(exits) == 4 and {row["name"] for row in exits} == INSTANCES
            and all(type(row["exit_code"]) is int and row["exit_code"] == 0 for row in exits), "driver exit mismatch")
    intervals = []
    for phase, result in phases:
        start = number(result["load_started_monotonic_seconds"])
        stop = start + number(result["load_stop_requested_elapsed_seconds"])
        intervals.append({"name": Path(phase["directory"]).name, "phase": phase["phase"], "start": start, "end": stop})
    shadows = [row for row in intervals if row["phase"] == "shadow"]
    common = {"start": max(row["start"] for row in shadows), "end": min(row["end"] for row in shadows)}
    require(common["end"] > common["start"], "no common shadow interval")
    require(all(common["start"] <= row["start"] <= row["end"] <= common["end"] for row in intervals if row["phase"] == "pressure"),
            "pressure outside actual common shadow coverage")
    by_name = {row["name"]: row for row in intervals}
    for mode in ("fast", "reliable"):
        ordered = [by_name[f"{mode}-pressure-{policy}"] for policy in POLICIES]
        require(all(before["end"] <= after["start"] for before, after in zip(ordered, ordered[1:])), "pressure phases overlap or run out of order")
        require(sum(row["end"] - row["start"] for row in ordered) >= 259200, "pressure effective duration below 72 hours")
    coverage = layout["coverage"]
    require(coverage["passed"] is True and coverage["errors"] == [], "recorded coverage failed")
    require(coverage["common_shadow_interval"] == common, "recorded common interval differs")
    require(sorted(coverage["intervals"], key=lambda row: row["name"]) == sorted(intervals, key=lambda row: row["name"]),
            "recorded actual intervals differ")
    return {"common_shadow_interval": common, "pressure_intervals": 10}


def validate_identity(root, phase, result, environment, summary, plan):
    """核对计划、原件和分析身份；Match planned, raw, and analyzed identities."""
    instance = f"{phase['event_mode']}-{phase['phase']}"
    frozen = plan["instances"][instance]
    substance = lambda settings: {key: value for key, value in settings.items() if key not in ("output", "server_binary")}
    require(substance(environment["settings"]) == substance(frozen["settings"]), "settings differ")
    source = PurePosixPath(plan["source_root"]) / instance
    require(environment["settings"]["output"] == str(source), "environment output source path differs")
    require(environment["script_sha256"] == frozen["script_sha256"] == sha256(root / instance / "tool_source/v030.py"), "driver script differs")
    require(environment["tool_sources_sha256"]["benchmarks/v020.py"] == frozen["v020_sha256"]
            == sha256(root / instance / "tool_source/v020.py"), "wire dependency differs")
    binary = environment["binary"]
    require(binary["copied_path"] == str(source / "ling"), "copied binary source path differs")
    require(environment["settings"]["server_binary"] == binary["input_path"], "binary input identity differs")
    require(binary["sha256"] == plan["binary"]["sha256"] == sha256(root / instance / "ling"), "binary bytes differ")
    require(binary["revision_asserted_by_operator"] == plan["binary"]["revision"], "binary revision differs")
    for key in ("event_mode", "phase", "policy", "seed", "requested_seconds"):
        require(result[key] == phase[key] == summary[key], "phase identity differs: " + key)
    require(summary["workers"] == frozen["settings"]["workers"], "analyzed workers differ")
    require(result["config_sha256"] == sha256(relative(root, phase["directory"]) / "ling.toml"), "config bytes differ")
    require(result["status"] == summary["status"] == "duration_reached", "phase did not reach duration")
    require(number(result["load_stop_requested_elapsed_seconds"]) >= phase["requested_seconds"], "actual load too short")
    return {"actual_load_seconds": result["load_stop_requested_elapsed_seconds"], "seed": phase["seed"]}


def validate_totals(base, phase, result, workers):
    """只用停止前后的负载边界；Use load boundary deltas, excluding post-stop drain."""
    first = base.snapshot_metrics(result["load_start"], workers, "load_start")
    last = base.snapshot_metrics(result["load_stop"], workers, "load_stop")
    totals = {}
    for name, minimum in phase["minimum_totals"].items():
        delta = integer(last[name]) - integer(first[name])
        require(delta >= integer(minimum), name + " load total below threshold")
        totals[name] = str(delta)
    cycle = phase["phase"] + "_cycles"
    start_counts, stop_counts = result["load_start"]["workload"]["counts"], result["load_stop"]["workload"]["counts"]
    delta = integer(stop_counts[cycle]) - integer(start_counts.get(cycle, 0))
    require(delta >= sum(integer(phase["minimum_totals"][f"worker_{i}_cycles"]) for i in range(workers)), "phase cycle total too low")
    totals[cycle] = str(delta)
    return totals


def validate_consumers(phase, result):
    """全阶段存活独立于采样下界；Whole-phase liveness is independent from sampled progress."""
    start, stop = result["load_start"], result["load_stop"]
    lower = number(stop["sample_started_elapsed_seconds"]) - number(start["sample_completed_elapsed_seconds"])
    upper = number(stop["sample_completed_elapsed_seconds"]) - number(start["sample_started_elapsed_seconds"])
    require(0 <= lower <= upper, "invalid load snapshot brackets")
    load_start = number(result["load_started_monotonic_seconds"])
    load_stop = load_start + number(result["load_stop_requested_elapsed_seconds"])
    require(set(start["consumers"]) == set(stop["consumers"]) == set(result["consumers"]) == set(GROUPS), "four exact consumers required")
    observed = {}
    for group in GROUPS:
        before, after, final = (row["consumers"][group] for row in (start, stop, result))
        require(before["termination"] == after["termination"] == "running" and final["termination"] == "requested_stop", group + " stopped early")
        require(before["errors"] == after["errors"] == final["errors"] == {}, group + " consumer error")
        received = integer(final["received"])
        require(received > 0 and received == integer(final["acks"]), group + " final delivery is not fully confirmed")
        require(integer(final["duplicates"]) <= received, group + " duplicate count exceeds deliveries")
        began = number(final["started_monotonic_seconds"])
        require(before["started_monotonic_seconds"] == after["started_monotonic_seconds"] == began, "consumer identity changed")
        require(began <= load_start and began + number(final["elapsed_seconds"]) >= load_stop, group + " does not cover full phase")
        active = number(after["active_seconds"]) - number(before["active_seconds"])
        offline = number(after["planned_disconnected_seconds"]) - number(before["planned_disconnected_seconds"])
        require(active >= 0 and offline >= 0 and lower - EPSILON <= active + offline <= upper + EPSILON,
                group + " active/offline does not match snapshot interval")
        reconnects = integer(after["reconnects"]) - integer(before["reconnects"])
        budget = phase["offline"][group]
        require(budget["min_reconnects"] <= reconnects <= budget["max_reconnects"], group + " reconnect budget violated")
        require(budget["min_seconds"] <= offline <= budget["max_seconds"], group + " offline budget violated")
        require(final["planned_offline"] is False, group + " final offline interval unclosed")
        observed[group] = {"active_seconds": active, "planned_offline_seconds": offline, "reconnects": reconnects}
    return observed


def validate_windows(phase, result, analyzed, limits):
    """检查每个完整60秒窗口，末尾碎片不补齐工作量；Check every full window without using the final fragment as progress."""
    windows, summary = analyzed["windows"], analyzed["summary"]
    end = number(result["load_stop_requested_elapsed_seconds"])
    require(len(windows) == math.ceil(end / 60), "window count differs from actual duration")
    full = 0
    evicted = 0
    for index, window in enumerate(windows):
        begin, stop = index * 60, min((index + 1) * 60, end)
        require(window["begin_seconds"] == begin and window["end_seconds"] == stop, "window timeline gap")
        is_full = stop - begin == 60
        require(window["full_window"] is is_full and window["offline_timeline_complete"] is True, "window classification/offline evidence invalid")
        for group in GROUPS:
            consumer = window["consumers"][group]
            states = consumer["sampled_termination_states"]
            require(isinstance(states, dict) and set(states) <= {"running"}, "sampled consumer is not running")
            if is_full:
                require(states and integer(states["running"]) > 0, "full window lacks consumer state")
                active = number(window["metrics"]["active_seconds." + group]["delta"])
                require(active >= phase["minimum_window_active_seconds"][group], "window active lower bound too low")
                require(active + number(consumer["planned_offline_seconds"]) + EPSILON >= number(window["observed_span_seconds"]), "sampled active/offline coverage gap")
        eviction = window["metrics"]["by_kind.Evicted"]["delta"]
        if eviction is not None:
            evicted += integer(eviction)
        if not is_full:
            continue
        full += 1
        for name, minimum in phase["minimum_window"].items():
            metric = window["metrics"][name]
            require(metric["unavailable_reason"] is None and metric["missing_samples"] == 0, "window metric unavailable: " + name)
            require(integer(metric["delta"]) >= integer(minimum), "window count below threshold: " + name)
        require(number(window["max_sample_start_gap_seconds"]) <= limits["max_sample_start_gap_seconds"], "window sample gap too long")
        require(number(window["max_sample_duration_seconds"]) <= limits["max_sample_duration_seconds"], "window sample too slow")
    require(summary["full_windows"] == full and summary["partial_windows"] == len(windows) - full, "summary window counts differ")
    for name in phase["minimum_window"]:
        minimum = summary["observed_minima"][name]
        require(minimum["usable_full_windows"] == full and minimum["unavailable_full_windows"] == 0, "unavailable full window hidden by minimum")
    return {"full_windows": full, "partial_windows": len(windows) - full, "evicted_lower_bound": str(evicted)}


def validate_resources(summary, limits, result):
    """资源值必须全程有效，零斜率合法；Require complete native observations; zero slopes are valid."""
    rows = integer(summary["resource_rows"])
    require(rows > 0 and integer(summary["resource_samples_omitted"]) == 0, "missing/omitted resource samples")
    require(rows == integer(result["resource_samples_saved"]) == integer(result["native_resources"]["attempts"]), "saved/attempted resource sample counts differ")
    for field in ("cpu_seconds", "rss_bytes", "handles", "threads"):
        state = summary["native_availability"][field]
        require(integer(state["valid"]) == rows and integer(state["missing"]) == 0, "native field unavailable: " + field)
        require(integer(result["native_resources"]["valid"][field]) == rows
                and integer(result["native_resources"]["missing"].get(field, 0)) == 0, "recorded native availability differs")
    require(number(summary["max_resource_start_gap_seconds"]) <= limits["max_sample_start_gap_seconds"], "resource sample gap too long")
    require(number(summary["max_resource_sample_duration_seconds"]) <= limits["max_sample_duration_seconds"], "resource sample too slow")
    trend = summary["resource_trends_recomputed"]
    require(trend["warmup_seconds"] == limits["warmup_seconds"], "resource warmup differs")
    slopes = {}
    for field, limit in (("rss_bytes", "rss_bytes_per_hour"), ("handles", "handles_per_hour"), ("threads", "threads_per_hour")):
        state = trend["fields"][field]
        slope = number(state["slope_per_hour"], nonnegative=False)
        require(integer(state["samples"]) >= 3 and integer(state["missing_samples"]) == 0 and number(state["span_seconds"]) >= 60, "insufficient steady-state resources")
        require(number(state["max_sample_start_gap_seconds"]) <= limits["max_sample_start_gap_seconds"] and slope <= limits[limit], field + " slope/gap exceeded")
        slopes[field] = slope
    return {"resource_rows": rows, "slopes_per_hour": slopes}


def validate_errors_and_capacity(phase, result, analyzed):
    """容量OOM与其他错误分开，所有触限证据限定在负载内。
    Separate expected capacity OOM from errors and bound capacity proof to the load interval.
    """
    summary = analyzed["summary"]
    require(result["findings"] == summary["recorded_findings"] == {}, "recorded findings exist")
    require(summary["diagnostics"]["counts"] == {} and summary["diagnostics"]["examples"] == [], "base analyzer diagnostics exist")
    require(all(row["complete"] is True for row in summary["source_files"].values()) and len(summary["source_files"]) == 3, "incomplete source stream")
    errors = result["server_errors"]
    require(isinstance(errors, dict) and summary["recorded_server_errors"] == errors, "server error evidence missing")
    allowed = allowed_errors(phase)
    require(set(errors) <= allowed and all(integer(count) > 0 for count in errors.values()), "JournalFull/IO/Unknown or unexpected server error")
    require(set(summary["offline"]["server_errors"]) <= allowed and summary["offline"]["timeline_complete"] is True, "offline/error timeline failed")
    require(result["server_exit_code"] == 0 and result["shutdown_method"] == "SHUTDOWN request", "server did not shut down cleanly")
    require(result["server_log_capture_complete"] is True and integer(result["server_panic_markers"]) == 0, "server log capture/panic failure")
    for name in ("journal_errors", "journal_compaction_failures"):
        state = summary["info_event_counter_observations"][name]
        require(state["missing_samples"] == 0 and state["regressed"] is False
                and all(integer(state[key]) == 0 for key in ("first", "last", "max")), "journal IO/failure counter is nonzero or unavailable")
    require(result["consumer_acceptance"]["valid"] is True and result["consumer_acceptance"]["failures"] == []
            and result["consumer_acceptance"]["expected_fast_lag"] == [], "consumer acceptance failed")
    for name in ("no_findings", "full_duration", "all_workers_completed", "consumers_valid", "native_resources_available"):
        require(result["criteria"][name] is True, "driver criterion failed: " + name)
    if phase["phase"] == "shadow":
        require(result["criteria"]["all_ten_types_checked"] is True and result["criteria"]["shadow_writes_not_capacity_limited"] is True,
                "shadow type/capacity criterion failed")
        counts = result["load_stop"]["workload"]["counts"]
        require(all(integer(counts["type_" + kind]) > 0 for kind in ("null", "bool", "int", "uint", "float", "string", "bytes", "list", "map", "set")), "ten types not completed during load")
    workers = len([name for name in phase["minimum_totals"] if name.startswith("worker_")])
    require(integer(result["counts"].get("workers_failed", 0)) == 0
            and integer(result["counts"]["workers_requested_stop"]) == workers, "worker termination counters differ")
    before, after = (result[name]["workload"]["server_errors"] for name in ("load_start", "load_stop"))
    oom = integer(after.get("ERR_OUT_OF_MEMORY", 0)) - integer(before.get("ERR_OUT_OF_MEMORY", 0))
    evicted = sum(integer(window["metrics"]["by_kind.Evicted"]["delta"]) for window in analyzed["windows"]
                  if window["metrics"]["by_kind.Evicted"]["delta"] is not None)
    minimum_oom, minimum_evicted = capacity_thresholds(phase)
    require(oom >= minimum_oom and evicted >= minimum_evicted, "policy did not prove capacity pressure")
    if phase["phase"] == "shadow":
        require(result["workload_degraded_by_resource_limit"] is False and oom == 0, "shadow reached capacity")
        counts = result["load_stop"]["workload"]["counts"]
        require(integer(counts.get("shadow_cycles_resource_limited", 0)) == 0, "shadow workload degraded")
    return {"load_oom": str(oom), "load_evicted_lower_bound": str(evicted), "server_errors": errors}


def allowed_errors(phase):
    """允许冻结驱动主动验证的拒绝；Allow the rejections intentionally exercised by the frozen driver."""
    if phase["phase"] == "pressure":
        return {"ERR_KEY_NOT_FOUND", "ERR_OUT_OF_MEMORY"}
    return {"ERR_KEY_NOT_FOUND", "ERR_OVERFLOW", "ERR_TYPE_MISMATCH", "ERR_INDEX_OUT_OF_RANGE"}


def validate_error_records(base, path, phase, result):
    """逐条检查expected标记，汇总不能掩盖意外错误；Check every expected flag, not just aggregate codes."""
    diagnostics = base.Diagnostics()
    reader, errors = base.JsonLines(path, diagnostics), Counter()
    for row in reader.rows():
        if row.get("category") == "server_error":
            require(row["expected"] is True and row["code"] in allowed_errors(phase), "unexpected/forbidden server error record")
            errors[row["code"]] += 1
        else:
            require(row.get("category") in {"planned_consumer_disconnect_started", "planned_consumer_disconnect_completed",
                                            "planned_consumer_reconnect", "duplicate_delivery"}, "finding or unknown error category")
    require(reader.stats["complete"] is True and not diagnostics.counts, "incomplete error records")
    require(dict(errors) == result["server_errors"], "raw server error totals differ")
    return dict(errors)


def validate_log(path):
    """完整日志也必须通过gzip尾部校验；Validate the complete server log through gzip EOF."""
    with gzip.open(path, "rb") as stream:
        while stream.read(1024 * 1024):
            pass
    return "gzip EOF and CRC verified"


def delivery_observations(base, result, summary):
    """原样保留尾部积压/丢弃/重复；不把正进展当作全部交付证明。
    Retain final backlog, drops, and duplicates without equating progress with complete delivery.
    """
    events = base.events_info({"info": result["final_info"]})
    return {"post_drain_info": {name: str(integer(base.wrapped(events, name))) for name in ("pending", "dropped", "redelivered", "committed")},
            "sampled_dropped": summary["info_event_counter_observations"]["dropped"],
            "post_drain_consumers": {group: {name: str(integer(result["consumers"][group][name])) for name in ("received", "acks", "duplicates")}
                                     for group in GROUPS},
            "scope": "counts and sampled identity validation only; not proof that every produced event was delivered or duplicate payloads were unchanged"}


def evaluate(plan_path, analysis_path, root):
    """先绑定证据，后独立判定，任何失败使整体失败；Bind sources before independent fail-closed acceptance checks."""
    checks = Checks()
    plan = read_json(plan_path)
    checks.run("plan.complete_and_frozen", str(plan_path), lambda: validate_plan(plan, root))
    if not checks.rows[-1]["passed"]:
        return {"format": "ling-v030-acceptance-evaluation-1", "passed": False, "checks": checks.rows, "evidence": {}}
    inventory = source_inventory(root, plan, plan_path, analysis_path)
    base = analyzer_module()
    checks.run("analysis.matches_current_sources", str(analysis_path), lambda: bind_analysis(base, root, plan, analysis_path))
    analysis = read_json(analysis_path)
    require(analysis["format"] == "ling-v030-observations-1" and analysis["window_seconds"] == 60, "wrong analysis format")
    phases = {}
    for analyzed in analysis["phases"]:
        key = str(Path(analyzed["directory"]).resolve())
        require(key not in phases, "duplicate analyzed phase")
        phases[key] = analyzed
    require(set(phases) == {str(relative(root, phase["directory"])) for phase in plan["phases"]}, "analysis phase set differs")
    raw = [(phase, read_json(relative(root, phase["directory"]) / "result.json")) for phase in plan["phases"]]
    checks.run("layout.recorded_source_paths", "layout.json", lambda: validate_source_paths(read_json(root / "layout.json"), plan, root))
    checks.run("layout.actual_coverage_and_clean_exit", "layout-result.json", lambda: validate_layout(read_json(root / "layout-result.json"), raw))
    for phase, result in raw:
        directory = relative(root, phase["directory"])
        analyzed = phases[str(directory)]
        environment = read_json(directory.parent / "environment.json")
        workers = environment["settings"]["workers"]
        for name, operation in (
            ("identity_and_duration", lambda: validate_identity(root, phase, result, environment, analyzed["summary"], plan)),
            ("load_boundary_totals", lambda: validate_totals(base, phase, result, workers)),
            ("whole_phase_consumers", lambda: validate_consumers(phase, result)),
            ("full_60_second_windows", lambda: validate_windows(phase, result, analyzed, plan["limits"])),
            ("native_resources_and_slopes", lambda: validate_resources(analyzed["summary"], plan["limits"], result)),
            ("zero_unexpected_errors_and_actual_capacity", lambda: validate_errors_and_capacity(phase, result, analyzed)),
            ("every_error_explicitly_expected", lambda: validate_error_records(base, directory / "errors.jsonl.gz", phase, result)),
            ("complete_server_log", lambda: validate_log(directory / "server-complete.log.gz")),
            ("delivery_observations", lambda: delivery_observations(base, result, analyzed["summary"]))):
            checks.run(phase["directory"] + "." + name, str(directory), operation)
    checks.run("evidence.unchanged_during_evaluation", "all inventoried sources",
               lambda: require(source_inventory(root, plan, plan_path, analysis_path) == inventory, "source bytes/file sizes changed"))
    return {"format": "ling-v030-acceptance-evaluation-1", "passed": all(row["passed"] for row in checks.rows),
            "root": str(root), "source_root": plan["source_root"], "run_id": plan["run_id"], "checks": checks.rows, "evidence": inventory,
            "scope": "frozen numerical acceptance; base analyzer reused; no journal/protocol implementation audit, all-event delivery proof, or duplicate payload comparison"}


def main(argv=None):
    """输出始终新建，原件不改；Create a fresh report without editing evidence."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--analysis", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    root = args.root.resolve()
    require(args.output.resolve() not in {args.plan.resolve(), args.analysis.resolve()} and not args.output.resolve().is_relative_to(root), "output must be outside original evidence")
    try:
        report = evaluate(args.plan.resolve(), args.analysis.resolve(), root)
    except (ValueError, KeyError, TypeError, IndexError, OSError, EOFError, OverflowError, zlib.error) as error:
        report = {"format": "ling-v030-acceptance-evaluation-1", "passed": False,
                  "checks": [{"check": "evidence.readable_and_complete", "passed": False,
                              "error": type(error).__name__ + ": " + str(error)}]}
    with args.output.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(report, stream, ensure_ascii=False, allow_nan=False, indent=2)
        stream.write("\n")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
