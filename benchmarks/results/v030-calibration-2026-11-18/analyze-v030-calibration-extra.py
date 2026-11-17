"""只读补充四实例校准证据，不判定计划验收。
Read-only supplementary observations for a complete four-instance calibration.

Run only after all drivers stop and the evidence has been copied locally:
  python -B target/analyze-v030-calibration-extra.py ROOT REPORT --output EXTRA.json
ROOT contains the four instance directories; REPORT is v030_analyze's JSON.
The output must not exist. No input is changed and no subprocess is started.
"""
import argparse
from collections import Counter
from decimal import Decimal, ROUND_CEILING
import gzip
import hashlib
import importlib.util
import json
import math
from pathlib import Path
import sys
import zlib


sys.dont_write_bytecode = True
ANALYZER_PATH = Path(__file__).resolve().parents[1] / "benchmarks" / "v030_analyze.py"
SPEC = importlib.util.spec_from_file_location("v030_observations", ANALYZER_PATH)
BASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BASE)
U64_MAX = 2**64 - 1
THRESHOLD = 3 * 2**29  # 4 GiB * 3/8 = 1.5 GiB event admission threshold.
PERIODS = (86400, 51840, 259500)
POLICIES = ("noeviction", "lru", "lfu", "random", "ttl-first")
EVENT_COUNTERS = ("dropped", "redelivered", "journal_errors", "journal_compactions",
                  "journal_compaction_successes", "journal_compaction_failures")
TX_FIELDS = ("journal_transaction_bytes.sum", "journal_transaction_bytes.count")


def metadata(path, diagnostics, required=True, limit=64 * 1024 * 1024):
    """限制元数据读取；Bound metadata reads without modifying source evidence."""
    if not required and not path.exists():
        return {}
    try:
        with path.open("rb") as stream:
            raw = stream.read(limit + 1)
        value = json.loads(raw) if len(raw) <= limit else None
        if not isinstance(value, dict):
            raise ValueError("expected a bounded JSON object")
        return value
    except (OSError, ValueError, UnicodeError, RecursionError) as error:
        diagnostics.add("unreadable_metadata", file=str(path), error=type(error).__name__)
        return {}


def digest(path):
    """流式摘要；Hash a file incrementally."""
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def point(value, bracket):
    """整数保留十进制字符串；Keep exact integer values as decimal strings."""
    return {"value": str(value) if type(value) is int else value,
            "start_seconds": bracket[0] if bracket else None,
            "end_seconds": bracket[1] if bracket else None}


class Observations:
    """固定字段的首尾、异常和相邻增量；Bounded observations for one metric."""
    def __init__(self, counter=True):
        self.counter = counter
        self.first = self.last = self.previous = None
        self.valid = self.missing = self.regressions = self.saturated = 0
        self.peak = None
        self.adjacent = None
        self.adjacent_lower = None
        self.changes = []
        self.change_count = 0

    def add(self, value, bracket):
        """坏值切断相邻链，不跨缺失样本计算速率。
        Invalid values break adjacency; missing observations are never bridged.
        """
        if value is None:
            self.missing += 1
            self.previous = None
            return
        item = (value, bracket)
        self.valid += 1
        self.saturated += int(self.counter and value == U64_MAX)
        if self.last is not None and value < self.last[0]:
            self.regressions += 1
        self.first = self.first or item
        self.last = item
        self.peak = item if self.peak is None or value > self.peak[0] else self.peak
        if self.previous and bracket and self.previous[1]:
            previous, before = self.previous
            delta = value - previous
            if delta and self.counter:
                self.change_count += 1
                if len(self.changes) < 20:
                    self.changes.append({"delta": str(delta), "start_seconds": bracket[0]})
            if delta >= 0 and bracket[0] > before[0]:
                candidate = {"delta": str(delta), "first": point(previous, before),
                             "last": point(value, bracket),
                             "start_span_seconds": bracket[0] - before[0],
                             "outer_bracket_span_seconds": bracket[1] - before[0],
                             "inner_bracket_span_seconds": bracket[0] - before[1]}
                rate = delta / candidate["start_span_seconds"]
                lower = delta / candidate["outer_bracket_span_seconds"]
                if delta > 0 and (self.adjacent is None or rate > self.adjacent["rate_per_second"]):
                    self.adjacent = {**candidate, "rate_per_second": rate}
                if delta > 0 and (self.adjacent_lower is None or lower > self.adjacent_lower["rate_per_second"]):
                    self.adjacent_lower = {**candidate, "rate_per_second": lower}
        self.previous = item if bracket else None

    def report(self, time_problems=0):
        """保留异常时的首尾值，但不把无效差分用于外推。
        Keep endpoints despite anomalies, withholding invalid extrapolation rates.
        """
        reasons = []
        if self.missing:
            reasons.append("missing_samples")
        if self.counter and self.regressions:
            reasons.append("counter_regressed")
        if self.saturated:
            reasons.append("counter_saturated")
        if time_problems:
            reasons.append("sample_time_problems")
        delta = None
        span = rate = None
        if self.first and self.last:
            delta = self.last[0] - self.first[0]
            if self.first[1] and self.last[1]:
                span = self.last[1][0] - self.first[1][0]
                if not reasons and span > 0 and delta >= 0:
                    rate = delta / span
        return {"first": point(*self.first) if self.first else None,
                "last": point(*self.last) if self.last else None,
                "delta": str(delta) if type(delta) is int else delta,
                "valid_samples": self.valid, "missing_samples": self.missing,
                "regressions": self.regressions, "saturated_samples": self.saturated,
                "peak": point(*self.peak) if self.peak else None,
                "sample_start_span_seconds": span,
                "whole_sample_start_normalized_rate_per_second": rate,
                "rate_unavailable_reasons": reasons,
                "positive_max_adjacent_start_normalized": self.adjacent,
                "positive_max_adjacent_bracket_lower": self.adjacent_lower,
                "positive_adjacent_rate_scope": "observed interval averages; neither is an instantaneous or future upper bound",
                "change_count": self.change_count, "first_changes": self.changes}


def tx_values(events):
    """与现有分析器相同，只拆每层一个 Value 包装。
    Use exactly one Value wrapper per layer, as the existing analyzer does.
    """
    histogram = BASE.wrapped(events, "journal_transaction_bytes")
    return {name: BASE.unsigned(BASE.wrapped(histogram, name.rsplit(".", 1)[1])) for name in TX_FIELDS}


def final_events(result):
    """只保留容量和消费判断所需的最终精确计数。
    Preserve only exact final counters and group values needed for this decision.
    """
    events = BASE.events_info({"info": result.get("final_info")})
    fields = ("committed", "dropped", "redelivered", "journal_errors", "journal_compactions",
              "journal_compaction_successes", "journal_compaction_failures", "journal_bytes", "pending")
    values = {name: BASE.wrapped(events, name) for name in fields}
    kinds = BASE.wrapped(events, "by_kind")
    values["by_kind"] = {name: item.get("value") for name, item in zip(BASE.KINDS, kinds)} if isinstance(kinds, list) and all(isinstance(item, dict) for item in kinds) else None
    values.update({name: str(value) if value is not None else None for name, value in tx_values(events).items()})
    return values


def counter_mapping(value):
    """完整保留工作量计数，不经浮点转换。
    Preserve the complete workload counter mapping without float conversion.
    """
    return {name: str(parsed) if (parsed := BASE.unsigned(item)) is not None else item
            for name, item in BASE.mapping(value).items()}


def consumers_final(result):
    """在原始最终状态上补唯一 ACK，不混同 ACK 调用数。
    Add exact unique acknowledgements to the recorded final consumer state.
    """
    return {name: {**BASE.mapping(value), "unique_confirmed_acks": str(count) if (count := BASE.unique_acks(value)) is not None else None}
            for name, value in BASE.mapping(result.get("consumers")).items()}


def gzip_integrity(path):
    """读取到 EOF 校验 gzip 尾部和 CRC，不将存在当作完成。
    Read through EOF and CRC; existence alone does not establish completion.
    """
    size = 0
    try:
        with gzip.open(path, "rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                size += len(block)
        return {"complete": True, "uncompressed_bytes": str(size)}
    except (OSError, EOFError, zlib.error) as error:
        return {"complete": False, "uncompressed_bytes_read": str(size), "error": type(error).__name__}


def projection(rate, baseline, peak):
    """1.25 倍历史速率情景，包含基线；不是容量上界证明。
    Extrapolate historical rates with 1.25 and baseline; this is not an upper-bound proof.
    """
    if rate is None or baseline is None or peak is None:
        return None
    result = {}
    for seconds in PERIODS:
        growth = int((Decimal(str(rate)) * Decimal("1.25") * seconds).to_integral_value(rounding=ROUND_CEILING))
        total = max(peak, baseline + growth)
        result[str(seconds)] = {"projected_appends_with_factor_bytes": str(growth),
                                "baseline_plus_appends_or_observed_peak_bytes": str(total),
                                "event_threshold_bytes": str(THRESHOLD),
                                "remaining_to_threshold_bytes": str(THRESHOLD - total),
                                "fraction_of_threshold": total / THRESHOLD}
    return result


def analyze_resources(directory, result, workers, diagnostics):
    """流式读取资源；只保留固定计数器和每个整分钟的首尾。
    Stream resources, retaining fixed metrics and two endpoints per full minute.
    """
    reader = BASE.JsonLines(directory / "resources.jsonl.gz", diagnostics)
    states = {name: Observations() for name in BASE.metric_names(workers) if not name.startswith("active_seconds.")}
    states.update({name: Observations() for name in (*EVENT_COUNTERS, *TX_FIELDS)})
    gauges = {name: Observations(False) for name in ("journal_active_bytes", "journal_directory_bytes")}
    load_end = BASE.number(result.get("load_stop_requested_elapsed_seconds"))
    full_count = math.floor(load_end / 60) if load_end is not None else 0
    windows = [{"begin_seconds": 60 * i, "end_seconds": 60 * (i + 1), "contained_samples": 0,
                "crossing_samples_excluded": 0, "fields": {name: Observations() for name in TX_FIELDS}}
               for i in range(full_count)]
    kinds, times, flags = Counter(), Counter(), Counter()
    first_bracket = last_bracket = previous = None
    largest_gap = largest_duration = 0.0
    for row in reader.rows():
        kind = row.get("sample_kind")
        kinds[str(kind)] += 1
        start, end = BASE.number(row.get("sample_started_elapsed_seconds")), BASE.number(row.get("sample_completed_elapsed_seconds"))
        bracket = (start, end) if start is not None and end is not None and end >= start else None
        if bracket is None:
            times["invalid_bracket"] += 1
        else:
            largest_duration = max(largest_duration, end - start)
            if previous:
                if start <= previous[0]:
                    times["reversed_or_duplicate_start"] += 1
                if start < previous[1]:
                    times["overlapping_bracket"] += 1
                largest_gap = max(largest_gap, start - previous[0])
            first_bracket = first_bracket or bracket
            last_bracket = previous = bracket
        if kind not in ("periodic", "terminal"):
            flags["invalid_sample_kind"] += 1
        events = BASE.events_info(row)
        values = BASE.snapshot_metrics(row, workers, "resource")
        for group in BASE.GROUPS:
            consumer = BASE.mapping(BASE.mapping(row.get("consumers")).get(group))
            components = [BASE.unsigned(value) for field in ("confirmed_watermarks", "confirmed_above_watermark")
                          for value in BASE.mapping(consumer.get(field)).values()]
            if any(value == U64_MAX for value in components):
                flags["consumer_ack_component_saturated." + group] += 1
                values["ack_unique." + group] = None
        values.update({name: BASE.unsigned(BASE.wrapped(events, name)) for name in EVENT_COUNTERS})
        tx = tx_values(events)
        values.update(tx)
        for name, state in states.items():
            state.add(values.get(name), bracket)
        gauges["journal_active_bytes"].add(BASE.unsigned(BASE.wrapped(events, "journal_bytes")), bracket)
        gauges["journal_directory_bytes"].add(BASE.unsigned(BASE.mapping(row.get("journal")).get("bytes")), bracket)
        for name in ("journal_changed_during_sample", "prometheus_truncated", "prometheus_unavailable", "info_unavailable"):
            if row.get(name):
                flags[name] += 1
        if BASE.mapping(row.get("journal")).get("truncated"):
            flags["journal_directory_truncated"] += 1
        if bracket and kind == "periodic" and windows:
            first_index = max(0, int(start // 60) - int(start == end and start > 0 and start % 60 == 0))
            last_index = min(len(windows) - 1, int(end // 60))
            indices = range(first_index, last_index + 1)
            for index in indices:
                if not 0 <= index < len(windows):
                    continue
                window = windows[index]
                if window["begin_seconds"] <= start and end <= window["end_seconds"]:
                    window["contained_samples"] += 1
                    for name, state in window["fields"].items():
                        state.add(tx[name], bracket)
                elif start < window["end_seconds"] and end > window["begin_seconds"]:
                    window["crossing_samples_excluded"] += 1
    time_problems = sum(times.values())
    observations = {name: state.report(time_problems) for name, state in states.items()}
    gauge_report = {name: state.report(time_problems) for name, state in gauges.items()}
    minimum, maximum, usable = {}, {}, Counter()
    for window in windows:
        reports = {}
        for name, state in window.pop("fields").items():
            report = state.report(time_problems)
            first, last = state.first, state.last
            reasons = list(report["rate_unavailable_reasons"])
            if observations[name]["rate_unavailable_reasons"]:
                reasons.append("whole_stream_counter_problem")
            if not first or not last or state.valid < 2 or first[1][1] > last[1][0]:
                reasons.append("insufficient_independent_samples")
            delta = last[0] - first[0] if not reasons else None
            rate = delta / 60 if delta is not None else None
            reports[name] = {"first": report["first"], "last": report["last"],
                             "delta_lower_bound": str(delta) if delta is not None else None,
                             "rate_lower_bound_per_second": rate, "unavailable_reasons": sorted(set(reasons))}
            if rate is not None:
                usable[name] += 1
                entry = {"begin_seconds": window["begin_seconds"], "delta_lower_bound": str(delta), "rate_lower_bound_per_second": rate}
                if name not in minimum or rate < minimum[name]["rate_lower_bound_per_second"]:
                    minimum[name] = entry
                if name not in maximum or rate > maximum[name]["rate_lower_bound_per_second"]:
                    maximum[name] = entry
        window["metrics"] = reports
    tx_sum = observations[TX_FIELDS[0]]
    baseline = gauges["journal_active_bytes"].first
    peak = gauges["journal_active_bytes"].peak
    baseline, peak = (baseline[0] if baseline else None), (peak[0] if peak else None)
    no_problems = all(not observations[name]["rate_unavailable_reasons"] for name in TX_FIELDS) and reader.stats["complete"]
    rates = {"whole_sample_start_normalized": tx_sum["whole_sample_start_normalized_rate_per_second"] if no_problems else None}
    adjacent = tx_sum["positive_max_adjacent_start_normalized"]
    rates["positive_max_adjacent_start_normalized"] = adjacent["rate_per_second"] if adjacent and no_problems else None
    rates["maximum_fixed_60s_lower_bound"] = maximum.get(TX_FIELDS[0], {}).get("rate_lower_bound_per_second") if no_problems else None
    prerequisites = {"resource_gzip_complete": reader.stats["complete"],
        "histogram_sum_and_count_usable": no_problems,
        "active_file_gauge_complete": gauges["journal_active_bytes"].missing == 0 and baseline is not None and peak is not None,
        "physical_directory_gauge_complete": gauges["journal_directory_bytes"].missing == 0,
        "no_directory_or_cross_sample_flags": not any(flags[name] for name in ("journal_directory_truncated", "journal_changed_during_sample")),
        "journal_error_counter_present_and_zero": states["journal_errors"].missing == 0 and states["journal_errors"].peak is not None and states["journal_errors"].peak[0] == 0,
        "journal_compaction_counters_complete": all(states[name].missing == 0 and states[name].regressions == 0 and states[name].saturated == 0 for name in EVENT_COUNTERS if "compaction" in name)}
    can_project = all(prerequisites.values())
    return {"source": {**reader.stats, "saved_rows_recorded": result.get("resource_samples_saved")},
            "sample_kinds": dict(kinds), "sample_time_problems": dict(times), "sample_flags": dict(flags),
            "first_sample_bracket": first_bracket, "last_sample_bracket": last_bracket,
            "maximum_sample_start_gap_seconds": largest_gap, "maximum_sample_duration_seconds": largest_duration,
            "terminal_is_last": bool(kinds["terminal"] == 1 and kind == "terminal") if reader.stats["rows"] else False,
            "terminal_end_after_load_stop_seconds": last_bracket[1] - load_end if last_bracket and load_end is not None else None,
            "counter_observations": observations, "journal_sampled_gauges": gauge_report,
            "fixed_60s_intervals": windows, "fixed_60s_summary": {"full_intervals": full_count, "usable_intervals": dict(usable),
                "minimum_lower_bounds": minimum, "maximum_lower_bounds": maximum,
                "scope": "wholly contained periodic brackets only; counter delta / 60; terminal excluded"},
            "capacity_scenarios": {"factor": 1.25, "initial_active_bytes": str(baseline) if baseline is not None else None,
                "observed_active_peak_bytes": str(peak) if peak is not None else None,
                "prerequisites_for_emitting_scenarios": prerequisites,
                "rates_bytes_per_second": rates,
                "projections": {name: projection(rate, baseline, peak) if can_project else None for name, rate in rates.items()},
                "scope": "historical-rate scenarios ignoring reclamation, not future capacity upper bounds or acceptance"}}


def common_coverage(phases):
    """按实际单调起止重算共同覆盖，仅报告观察关系。
    Recompute overlap from actual monotonic load intervals, without declaring acceptance.
    """
    intervals, missing = [], []
    for phase in phases:
        result = phase["result"]
        start = BASE.number(result.get("load_started_monotonic_seconds"))
        duration = BASE.number(result.get("load_stop_requested_elapsed_seconds"))
        if start is None or duration is None or not math.isfinite(start + duration):
            missing.append(phase["name"])
        else:
            intervals.append({"name": phase["name"], "phase": result.get("phase"), "start": start, "end": start + duration})
    shadows = [item for item in intervals if item["phase"] == "shadow"]
    common = {"start": max(item["start"] for item in shadows), "end": min(item["end"] for item in shadows)} if len(shadows) == 2 else None
    if common:
        common["seconds"] = max(0, common["end"] - common["start"])
    pressure = []
    for item in intervals:
        if item["phase"] == "pressure":
            pressure.append({**item, "contained_in_common_shadow_interval": bool(common and common["start"] <= item["start"] <= item["end"] <= common["end"])})
    return {"common_shadow_interval": common, "shadow_intervals": shadows, "pressure_intervals": pressure,
            "missing_actual_intervals": missing, "scope": "actual clock coverage only, not phase or resource acceptance"}


def main():
    """只读取完整本地证据，独占创建一个补充 JSON。
    Read local evidence and exclusively create one supplementary JSON output.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("report", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    if args.output.exists():
        parser.error("output must not exist")
    diagnostics = BASE.Diagnostics()
    report = metadata(args.report, diagnostics)
    if report.get("format") != "ling-v030-observations-1" or report.get("window_seconds") != 60:
        diagnostics.add("unexpected_analysis_format")
    indexed = {}
    indexed_paths = {}
    report_phases = report.get("phases")
    if not isinstance(report_phases, list):
        diagnostics.add("analysis_phase_list_missing")
        report_phases = []
    for phase in report_phases:
        summary = BASE.mapping(BASE.mapping(phase).get("summary"))
        key = tuple(summary.get(name) for name in ("event_mode", "phase", "policy"))
        if key in indexed:
            diagnostics.add("duplicate_analysis_phase", phase=str(key))
        indexed[key] = summary
        indexed_paths[key] = BASE.mapping(phase).get("directory")
    layout = metadata(root / "layout-result.json", diagnostics)
    output_phases, all_gzip_complete = [], True
    for mode in ("fast", "reliable"):
        for phase in ("shadow", "pressure"):
            instance = root / f"{mode}-{phase}"
            environment = metadata(instance / "environment.json", diagnostics)
            settings = BASE.mapping(environment.get("settings"))
            workers = settings.get("workers")
            if type(workers) is not int or not 2 <= workers <= 16:
                diagnostics.add("invalid_worker_count", instance=instance.name)
                workers = 0
            for policy in (("noeviction",) if phase == "shadow" else POLICIES):
                name = f"{mode}-{phase}-{policy}"
                directory = instance / name
                result = metadata(directory / "result.json", diagnostics)
                key = (mode, phase, policy)
                if tuple(result.get(name) for name in ("event_mode", "phase", "policy")) != key:
                    diagnostics.add("phase_identity_mismatch", phase=name)
                analysis = indexed.pop(key, None)
                if analysis is None:
                    diagnostics.add("missing_analysis_phase", phase=name)
                    analysis = {}
                resource_diagnostics = BASE.Diagnostics()
                recorded_path = indexed_paths.get(key)
                normalized = str(recorded_path).replace("\\", "/").rstrip("/") if isinstance(recorded_path, str) else ""
                if [part for part in normalized.split("/") if part][-2:] != [instance.name, name]:
                    resource_diagnostics.add("analysis_relative_phase_path_mismatch")
                for field in ("event_mode", "phase", "policy", "seed", "status", "requested_seconds", "load_stop_requested_elapsed_seconds"):
                    if analysis.get(field) != result.get(field):
                        resource_diagnostics.add("analysis_metadata_mismatch", field=field)
                if analysis.get("workers") != workers:
                    resource_diagnostics.add("analysis_metadata_mismatch", field="workers")
                if settings.get("journal_bytes") != 4 * 2**30:
                    resource_diagnostics.add("journal_configuration_differs_from_4gib_reference")
                for field, recorded in (("findings", "recorded_findings"), ("server_errors", "recorded_server_errors"), ("consumers", "recorded_consumers")):
                    if analysis.get(recorded) != result.get(field):
                        resource_diagnostics.add("analysis_result_mismatch", field=field)
                resources = analyze_resources(directory, result, workers, resource_diagnostics)
                gzip_results = {"resources.jsonl.gz": resources["source"]}
                for filename in ("errors.jsonl.gz", "consumer-events.jsonl.gz", "server-complete.log.gz"):
                    gzip_results[filename] = gzip_integrity(directory / filename)
                for path in sorted(directory.glob("*.gz")):
                    if path.name not in gzip_results:
                        gzip_results[path.name] = gzip_integrity(path)
                phase_complete = all(item.get("complete") is True for item in gzip_results.values())
                all_gzip_complete = all_gzip_complete and phase_complete
                if not phase_complete:
                    diagnostics.add("unfinished_or_invalid_gzip", phase=name)
                reference_matches = settings.get("journal_bytes") == 4 * 2**30
                reliable_mode = mode == "reliable"
                resources["capacity_scenarios"]["prerequisites_for_emitting_scenarios"].update(
                    {"configured_journal_is_4gib_reference": reference_matches, "all_phase_gzip_complete": phase_complete,
                     "reliable_mode_has_a_journal": reliable_mode})
                if not reference_matches or not phase_complete or not reliable_mode:
                    resources["capacity_scenarios"]["projections"] = dict.fromkeys(resources["capacity_scenarios"]["projections"])
                if resources["source"]["rows"] != result.get("resource_samples_saved"):
                    resource_diagnostics.add("saved_resource_row_count_mismatch")
                if resources["source"]["rows"] != analysis.get("resource_rows"):
                    resource_diagnostics.add("analysis_resource_row_count_mismatch")
                omitted = BASE.unsigned(analysis.get("resource_samples_omitted"))
                if omitted is None or omitted:
                    resource_diagnostics.add("resource_samples_omitted_or_unknown")
                output_phases.append({"name": name, "relative_directory": str(directory.relative_to(root)),
                    "settings": settings, "result": {name: result.get(name) for name in
                        ("event_mode", "phase", "policy", "seed", "status", "requested_seconds", "load_started_monotonic_seconds",
                         "load_stop_requested_elapsed_seconds", "elapsed_seconds", "server_exit_code", "shutdown_method", "criteria",
                         "counts", "findings", "server_errors", "consumers", "consumer_acceptance", "workload_degraded_by_resource_limit",
                         "server_log_capture_complete", "server_panic_markers")},
                    "complete_workload_counts": counter_mapping(result.get("counts")),
                    "load_stop_workload_counts": counter_mapping(BASE.mapping(BASE.mapping(result.get("load_stop")).get("workload")).get("counts")),
                    "consumer_final_state": consumers_final(result),
                    "final_event_values": final_events(result),
                    "analysis": {name: analysis.get(name) for name in ("observed_minima", "full_windows", "partial_windows", "native_availability",
                        "resource_trends_recomputed", "info_event_counter_observations", "offline", "source_files", "diagnostics")},
                    "resources": resources, "gzip_integrity": gzip_results,
                    "supplementary_diagnostics": resource_diagnostics.summary()})
    if indexed:
        diagnostics.add("unexpected_analysis_phases", phases=[str(key) for key in indexed])
    diagnostic_phases = sum(bool(phase["supplementary_diagnostics"]["counts"]) for phase in output_phases)
    integrity = {"all_phase_gzip_eof_crc_complete": all_gzip_complete, "expected_phase_count": 12,
                 "phases_with_supplementary_diagnostics": diagnostic_phases,
                 "analysis_phase_count": len(report_phases), "supplementary_phase_count": len(output_phases)}
    result = {"format": "ling-v030-calibration-supplement-1", "root": str(root), "analysis_report": str(args.report.resolve()),
        "source_sha256": {"helper": digest(Path(__file__)), "analyzer": digest(ANALYZER_PATH), "analysis_report": digest(args.report)},
        "scope": "observations and historical-rate projections only; no plan thresholds chosen and no acceptance asserted",
        "journal_limits": {"configured_reference_bytes": str(4 * 2**30), "event_total_active_threshold_bytes": str(THRESHOLD),
            "control_total_active_threshold_bytes": str(2 * 2**30), "replacement_reserve_bytes": str(2 * 2**30)},
        "limitations": [
            "journal_transaction_bytes.sum/count share event and control appends: counted after validation before I/O, including uncertain attempts; no split and no compaction rewrite bytes",
            "journal_bytes is total active file bytes including retained events, historical controls, checkpoint and headers; it can shrink at compaction",
            "1.5 GiB is the total active-file event admission threshold for a 4 GiB setting; control records share it, rather than providing an additional 1.5 GiB",
            "directory peaks are sampled observations; short-lived old/new-file coexistence may be missed",
            "1.25 is a scenario factor, not a statistical confidence bound; linear totals ignore reclamation and do not reserve an extra maximum record/checkpoint",
            "the maximum of observed peak and baseline plus factored growth is an estimate, not proof of capacity or inevitable exhaustion",
            "60-second rates are conservative lower bounds from complete contained periodic brackets; they are not upper bounds for capacity planning",
            "whole-stream rates and peaks include terminal sampling after load stop; consumer active time is liveness, not proof of ACK progress",
            "a successful gzip EOF/CRC read verifies this local copy, not proof that a producer recorded every intended row; compare omissions, saved counts and analyzer diagnostics",
            "the upstream analyzer report has no per-input digest; metadata, path suffixes, final consumers and row counts are cross-checked but are not cryptographic proof of its source binding"],
        "integrity": integrity, "diagnostics": diagnostics.summary(),
        "recorded_layout_result": {name: layout.get(name) for name in ("orchestration_passed", "errors", "coverage", "driver_exits", "cleanup_events", "remaining_processes")},
        "actual_common_coverage": common_coverage(output_phases), "phases": output_phases}
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(result, stream, ensure_ascii=False, allow_nan=False, indent=2)
        stream.write("\n")
    print(json.dumps({"output": str(args.output.resolve()), **integrity, "diagnostics": diagnostics.summary()}, ensure_ascii=False))
    return 0 if all_gzip_complete and not diagnostics.counts and not diagnostic_phases else 2


if __name__ == "__main__":
    raise SystemExit(main())
