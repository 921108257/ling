"""流式汇总 0.3 校准与长测证据，不制定门槛或判定通过。
Stream 0.3 calibration/endurance evidence without choosing thresholds or declaring a pass.

Usage: python benchmarks/v030_analyze.py DIR [DIR ...] --output report.json
DIR is a phase directory or a run root. Count deltas/minima are decimal strings;
seconds remain JSON numbers. Memory is bounded by one input row and one window.
"""
import argparse
from collections import Counter
import gzip
import heapq
import json
import math
from pathlib import Path
import tempfile
import zlib


WINDOW_SECONDS = 60
MAX_ROW_BYTES = 8 * 1024 * 1024
GROUPS = tuple(f"{speed}-db{db}" for db in range(2) for speed in ("fast", "slow"))
KINDS = ("Created", "Updated", "Expired", "Deleted", "Evicted", "TtlChanged")
EVENT_KINDS = {"KEY_CREATED", "KEY_UPDATED", "KEY_EXPIRED", "KEY_DELETED", "KEY_EVICTED", "KEY_TTL_CHANGED"}


class Diagnostics:
    """固定数量类别与样例，避免错误日志决定内存用量。
    Bound diagnostic categories and examples independently of log size.
    """
    def __init__(self):
        self.counts, self.examples = Counter(), []

    def add(self, code, **details):
        """保留计数和前二十个小样例；Retain counts and the first twenty small examples."""
        self.counts[code] += 1
        if len(self.examples) < 20:
            self.examples.append({"code": code, **details})

    def summary(self):
        """只输出观察诊断，不转成验收结论；Report observations, not acceptance decisions."""
        return {"counts": dict(self.counts), "examples": self.examples}


def bounded_count(counter, key):
    """外部日志字符串不能创建无限类别；External strings cannot create unbounded categories."""
    key = key[:128]
    counter[key if key in counter or len(counter) < 128 else "<other>"] += 1


class JsonLines:
    """逐行读取 gzip，截断、坏行与过大行均使完整性诊断可见。
    Read gzip incrementally and expose truncation, malformed rows, and oversized rows.
    """
    def __init__(self, path, diagnostics):
        self.path, self.diagnostics = path, diagnostics
        self.stats = {"rows": 0, "complete": True}

    def problem(self, code, **details):
        """标记当前文件不完整；Mark this file's integrity as incomplete."""
        self.stats["complete"] = False
        self.diagnostics.add(code, file=self.path.name, **details)

    def rows(self):
        """最大只保留一行，超过限制停止此输入；Keep one row and stop an oversized input."""
        try:
            with gzip.open(self.path, "rb") as stream:
                line_number = 0
                while True:
                    raw = stream.readline(MAX_ROW_BYTES + 1)
                    if not raw:
                        break
                    line_number += 1
                    if len(raw) > MAX_ROW_BYTES:
                        self.problem("row_too_large", line=line_number)
                        break
                    if not raw.endswith(b"\n"):
                        self.problem("unterminated_json_line", line=line_number)
                    try:
                        row = json.loads(raw)
                        if not isinstance(row, dict):
                            raise ValueError("object required")
                    except (ValueError, UnicodeError, RecursionError):
                        self.problem("invalid_json_line", line=line_number)
                        continue
                    self.stats["rows"] += 1
                    yield row
        except (OSError, EOFError, zlib.error) as error:
            self.problem("unreadable_gzip", error=type(error).__name__)


def read_json(path, diagnostics, required=True):
    """元数据也有大小上限；Apply a size bound to metadata as well."""
    if not required and not path.exists():
        return {}
    try:
        with path.open("rb") as stream:
            raw = stream.read(MAX_ROW_BYTES + 1)
        if len(raw) > MAX_ROW_BYTES:
            raise ValueError("metadata too large")
        value = json.loads(raw)
        if not isinstance(value, dict):
            raise ValueError("object required")
        return value
    except (OSError, ValueError, UnicodeError, RecursionError) as error:
        diagnostics.add("unreadable_metadata", file=path.name, error=type(error).__name__)
        return {}


def number(value):
    """时间必须有限且非负，bool 不充作数值；Times must be finite, nonnegative numbers, not bools."""
    try:
        return value if type(value) in (int, float) and math.isfinite(value) and value >= 0 else None
    except OverflowError:
        return None


def unsigned(value):
    """读取十进制整数字符串而不经浮点；Read decimal integers without a float conversion."""
    if type(value) is int and 0 <= value < 2**64:
        return value
    if type(value) is str and len(value) <= 20 and value.isascii() and value.isdecimal():
        parsed = int(value)
        return parsed if parsed < 2**64 else None
    return None


def mapping(value):
    """错误类型等同缺失，不能让一行坏证据终止后续扫描。
    Treat a wrong object type as missing without aborting the remaining scan.
    """
    return value if isinstance(value, dict) else {}


def wrapped(mapping, name):
    """INFO 的 Value 包装只拆一层；Unwrap exactly one INFO Value layer."""
    field = mapping.get(name) if isinstance(mapping, dict) else None
    return field.get("value") if isinstance(field, dict) else None


def events_info(row):
    """缺失或错误 INFO 不伪装成空计数；Missing/error INFO does not become zero counters."""
    return wrapped(wrapped(row, "info"), "events") or {}


def unique_acks(consumer):
    """四分片水位加已确认空洞数；Sum four shard watermarks and confirmed holes, not ACK calls."""
    consumer = mapping(consumer)
    watermarks, holes = consumer.get("confirmed_watermarks"), consumer.get("confirmed_above_watermark")
    if not isinstance(watermarks, dict) or not isinstance(holes, dict) or set(watermarks) != set(map(str, range(4))) or set(holes) != set(watermarks):
        return None
    values = [unsigned(value) for value in (*watermarks.values(), *holes.values())]
    if any(value is None or value >= 2**64 for value in values):
        return None
    return sum(values)


def metric_names(workers):
    """固定指标集合也固定内存需求；A fixed metric set fixes memory requirements."""
    return ([f"worker_{index}_cycles" for index in range(workers)] + ["producer_write_confirmations", "committed"]
            + ["by_kind." + kind for kind in KINDS]
            + ["ack_unique." + group for group in GROUPS] + ["active_seconds." + group for group in GROUPS])


def snapshot_metrics(row, workers, source):
    """仅生产前边界允许尚未出现的工作量计数为零。
    Only the pre-production boundary permits absent workload counters to mean zero.
    """
    counts = mapping(mapping(row.get("workload")).get("counts"))
    consumers, events = mapping(row.get("consumers")), events_info(row)
    result = {name: unsigned(counts.get(name, 0 if source == "load_start" else None))
              for name in [f"worker_{index}_cycles" for index in range(workers)] + ["producer_write_confirmations"]}
    result["committed"] = unsigned(wrapped(events, "committed"))
    kinds = wrapped(events, "by_kind")
    for index, kind in enumerate(KINDS):
        value = kinds[index].get("value") if isinstance(kinds, list) and len(kinds) == 6 and isinstance(kinds[index], dict) else None
        result["by_kind." + kind] = unsigned(value)
    for group in GROUPS:
        consumer = mapping(consumers.get(group))
        result["ack_unique." + group] = unique_acks(consumer)
        result["active_seconds." + group] = number(consumer.get("active_seconds"))
    return result


class OfflineSweep:
    """一次扫描错误日志，把每组有效区间暂存磁盘，内存只留四条预读区间。
    Scan errors once, spooling valid intervals to disk with four lookahead intervals in memory.
    """
    def __init__(self, reader, phase_started, consumers, diagnostics):
        self.reader, self.phase_started, self.diagnostics = reader, phase_started, diagnostics
        self.categories, self.server_errors, self.intervals = Counter(), Counter(), Counter()
        self.durations, self.reconnects = Counter(), Counter()
        self.valid = phase_started is not None
        self.files = {group: tempfile.TemporaryFile(mode="w+t", encoding="utf-8") for group in GROUPS}
        opened, previous = {}, {}
        for row in reader.rows():
            category = row.get("category")
            bounded_count(self.categories, category if isinstance(category, str) else "<missing>")
            if category == "server_error":
                bounded_count(self.server_errors, str(row.get("code", "<missing>")))
            if category not in ("planned_consumer_disconnect_started", "planned_consumer_disconnect_completed"):
                continue
            group, started = row.get("consumer"), category.endswith("started")
            absolute = number(row.get("monotonic_seconds" if started else "completed_monotonic_seconds"))
            if group not in GROUPS or absolute is None or phase_started is None:
                diagnostics.add("invalid_offline_event")
                self.valid = False
                continue
            timestamp = absolute - phase_started
            # 各组由同一线程顺序记录，不要求不同组在日志锁前采集的时间全局有序。
            # A group's thread is ordered; timestamps captured before the log lock need not be globally ordered.
            if group in previous and timestamp < previous[group]:
                diagnostics.add("offline_time_reversed", consumer=group)
                self.valid = False
                continue
            previous[group] = timestamp
            if started:
                if group in opened:
                    diagnostics.add("overlapping_offline_start", consumer=group)
                    self.valid = False
                else:
                    opened[group] = timestamp
                continue
            stated, duration = number(row.get("started_monotonic_seconds")), number(row.get("duration_seconds"))
            reconnected = row.get("reconnected")
            actual_start = opened.pop(group, None)
            if (actual_start is None or stated is None or duration is None or type(reconnected) is not bool or timestamp < actual_start
                    or not math.isclose(stated - phase_started, actual_start, rel_tol=0, abs_tol=1e-6)
                    or not math.isclose(duration, timestamp - actual_start, rel_tol=0, abs_tol=1e-6)):
                diagnostics.add("unmatched_offline_completion", consumer=group)
                self.valid = False
            else:
                self.files[group].write(json.dumps([actual_start, timestamp]) + "\n")
                self.intervals[group] += 1
                self.durations[group] += duration
                self.reconnects[group] += int(reconnected)
        if opened:
            self.valid = False
            for group in opened:
                diagnostics.add("unclosed_offline_interval", consumer=group)
        for group in GROUPS:
            final = mapping(mapping(consumers).get(group))
            duration, reconnects = number(final.get("planned_disconnected_seconds")), unsigned(final.get("reconnects"))
            if (duration is None or reconnects is None or final.get("planned_offline") is not False
                    or not math.isclose(duration, self.durations[group], rel_tol=0, abs_tol=1e-6)
                    or reconnects != self.reconnects[group]):
                diagnostics.add("offline_consumer_totals_mismatch_or_missing", consumer=group)
                self.valid = False
        self.valid = self.valid and reader.stats["complete"]
        self.current = {}
        for group, stream in self.files.items():
            stream.seek(0)
            self.advance(group)

    def advance(self, group):
        """每组只预读一个已校验区间；Read one previously validated interval per group."""
        row = self.files[group].readline()
        self.current[group] = json.loads(row) if row else None

    def window(self, begin, end):
        """窗口仅扣除交集；损坏时间线返回未知而不猜测。
        Deduct intersections only; damaged timelines return unknown instead of a guessed duration.
        """
        totals = dict.fromkeys(GROUPS, 0.0)
        for group in GROUPS:
            interval = self.current[group]
            while interval is not None and interval[0] < end:
                totals[group] += max(0, min(end, interval[1]) - max(begin, interval[0]))
                if interval[1] > end:
                    break
                self.advance(group)
                interval = self.current[group]
        return totals if self.valid else dict.fromkeys(GROUPS)

    def finish(self):
        """关闭临时区间文件并输出完整性；Close interval spools and report integrity."""
        for stream in self.files.values():
            stream.close()
        return {"event_pairs": dict(self.intervals), "timeline_complete": self.valid,
                "total_seconds": dict(self.durations), "reconnects": dict(self.reconnects),
                "categories": dict(self.categories), "server_errors": dict(self.server_errors)}


class ResourceTrends:
    """按实际单调时刻、独立预热期做常量内存最小二乘。
    Fit constant-memory least squares at actual monotonic times after each phase's warmup.
    """
    def __init__(self, warmup):
        self.warmup = warmup
        self.fields = {name: {"samples": 0, "missing_samples": 0, "mean_x": 0.0, "mean_y": 0.0,
                              "sxx": 0.0, "sxy": 0.0, "first": None, "last": None, "peak": None, "max_gap": 0}
                       for name in ("rss_bytes", "handles", "threads")}

    def add(self, elapsed, native):
        """缺字段独立计数，不填零；Count missing fields independently without substituting zero."""
        if self.warmup is None or elapsed < self.warmup:
            return
        for name, state in self.fields.items():
            y = number(mapping(native).get(name))
            if y is None:
                state["missing_samples"] += 1
                continue
            x = elapsed - self.warmup
            state["samples"] += 1
            dx, dy = x - state["mean_x"], y - state["mean_y"]
            state["mean_x"] += dx / state["samples"]
            state["mean_y"] += dy / state["samples"]
            state["sxx"] += dx * (x - state["mean_x"])
            state["sxy"] += dx * (y - state["mean_y"])
            if state["first"] is None:
                state["first"] = x
            if state["last"] is not None:
                state["max_gap"] = max(state["max_gap"], x - state["last"])
            state["last"] = x
            state["peak"] = y if state["peak"] is None else max(state["peak"], y)

    def report(self):
        """至少三点、六十秒且时间方差非零才输出斜率。
        Slopes require three samples, sixty seconds, and nonzero time variance.
        """
        fields = {}
        for name, state in self.fields.items():
            span = state["last"] - state["first"] if state["first"] is not None else 0
            fields[name] = {"samples": state["samples"], "missing_samples": state["missing_samples"], "span_seconds": span,
                            "peak": state["peak"], "max_sample_start_gap_seconds": state["max_gap"],
                            "slope_per_hour": state["sxy"] / state["sxx"] * 3600
                            if state["samples"] >= 3 and span >= 60 and state["sxx"] > 0 else None}
        return {"warmup_seconds": self.warmup, "fields": fields}


class Samples:
    """按时间归档原始资源信息并追踪固定指标的回退。
    Archive resource observations and track regressions in a fixed set of metrics.
    """
    def __init__(self, reader, result, workers, diagnostics, warmup):
        self.reader, self.result, self.workers, self.diagnostics = reader, result, workers, diagnostics
        self.trends = ResourceTrends(warmup)
        self.previous, self.bad_since, self.peaks = {}, {}, {}
        self.native_availability = {name: {"valid": 0, "missing": 0} for name in ("cpu_seconds", "rss_bytes", "handles", "threads")}
        self.info_counters = {name: {"first": None, "last": None, "max": None, "missing_samples": 0, "regressed": False}
                              for name in ("dropped", "redelivered", "journal_errors", "journal_compactions",
                                           "journal_compaction_successes", "journal_compaction_failures")}
        self.resource_rows, self.max_duration, self.max_start_gap = 0, 0, 0
        self.last_resource_start = None

    def bracket(self, row, source):
        """旧版无区间时间时不退回 elapsed_seconds；Never substitute legacy elapsed_seconds for a bracket."""
        start, end = number(row.get("sample_started_elapsed_seconds")), number(row.get("sample_completed_elapsed_seconds"))
        if start is None or end is None or end < start:
            self.diagnostics.add("invalid_sample_bracket", source=source)
            return None
        return {"start": start, "end": end, "source": source, "row": row}

    def resources(self):
        """终止样本参与资源峰值，窗口计数稍后单独排除。
        Include terminal samples in resource peaks, excluding their counts from load windows later.
        """
        previous = None
        for row in self.reader.rows():
            if row.get("sample_kind") not in ("periodic", "terminal"):
                self.diagnostics.add("sample_kind_missing")
            sample = self.bracket(row, "terminal" if row.get("sample_kind") == "terminal" else "resource")
            if sample is None:
                continue
            start, end = sample["start"], sample["end"]
            if previous is not None and start < previous:
                self.diagnostics.add("resource_time_reversed", seconds=start)
                continue
            if previous is not None and start == previous:
                self.diagnostics.add("duplicate_resource_timestamp", seconds=start)
            previous = start
            self.resource_rows += 1
            self.max_duration = max(self.max_duration, end - start)
            if self.last_resource_start is not None:
                self.max_start_gap = max(self.max_start_gap, start - self.last_resource_start)
            self.last_resource_start = start
            for field, state in self.native_availability.items():
                state["missing" if number(mapping(row.get("native")).get(field)) is None else "valid"] += 1
            self.trends.add(start, row.get("native"))
            events = events_info(row)
            for name, state in self.info_counters.items():
                value = unsigned(wrapped(events, name))
                if value is None:
                    state["missing_samples"] += 1
                    continue
                if state["last"] is not None and value < state["last"]:
                    state["regressed"] = True
                    self.diagnostics.add("counter_regressed", metric="info.events." + name, seconds=start)
                state["first"] = value if state["first"] is None else state["first"]
                state["last"] = value
                state["max"] = value if state["max"] is None else max(state["max"], value)
            for key, value in (("journal_active_bytes", unsigned(wrapped(events, "journal_bytes"))),
                               ("journal_directory_bytes", unsigned(mapping(row.get("journal")).get("bytes")))):
                if value is not None and (key not in self.peaks or value > self.peaks[key]["bytes"]):
                    self.peaks[key] = {"bytes": value, "sample_started_elapsed_seconds": start, "sample_kind": sample["source"]}
            for flag in ("journal_changed_during_sample", "prometheus_truncated", "prometheus_unavailable"):
                if row.get(flag):
                    self.diagnostics.add(flag, seconds=start)
            if mapping(row.get("journal")).get("truncated"):
                self.diagnostics.add("journal_directory_truncated", seconds=start)
            yield sample

    def ordered(self):
        """只合并两条边界和流式资源输入；Merge two boundaries with the resource stream."""
        boundaries = [self.bracket(self.result.get(name, {}), name) for name in ("load_start", "load_stop")]
        boundaries = sorted((row for row in boundaries if row is not None), key=lambda row: row["start"])
        for sample in heapq.merge(boundaries, self.resources(), key=lambda row: row["start"]):
            values = snapshot_metrics(sample["row"], self.workers, sample["source"])
            sample["metrics"] = values
            for name, value in values.items():
                if value is None:
                    continue
                if name in self.previous and value < self.previous[name]:
                    self.diagnostics.add("counter_regressed", metric=name, seconds=sample["start"])
                    self.bad_since.setdefault(name, sample["start"])
                self.previous[name] = value
            yield sample


class Window:
    """每个窗口最多保留各指标首尾和四组状态计数。
    Retain only each metric's endpoints and four groups' observations per window.
    """
    def __init__(self, begin, end, names):
        self.begin, self.end = begin, end
        self.metrics = {name: {"first": None, "last": None, "missing_samples": 0} for name in names}
        self.first, self.last, self.count, self.crossing, self.max_gap, self.max_duration = None, None, 0, 0, 0, 0
        self.max_start_gap = 0
        self.statuses = {group: Counter() for group in GROUPS}

    def add(self, sample):
        """仅接纳完整落在窗口内的快照；Accept only snapshots wholly contained in the window."""
        self.count += 1
        if self.last is not None:
            self.max_gap = max(self.max_gap, sample["start"] - self.last["end"])
            self.max_start_gap = max(self.max_start_gap, sample["start"] - self.last["start"])
        self.first, self.last = self.first or sample, sample
        self.max_duration = max(self.max_duration, sample["end"] - sample["start"])
        for name, state in self.metrics.items():
            value = sample["metrics"][name]
            if value is None:
                if sample["source"] == "resource" or not (name == "committed" or name.startswith("by_kind.")):
                    state["missing_samples"] += 1
                continue
            point = (value, sample["start"], sample["end"])
            state["first"], state["last"] = state["first"] or point, point
        for group in GROUPS:
            status = mapping(mapping(sample["row"].get("consumers")).get(group)).get("termination", "<missing>")
            bounded_count(self.statuses[group], status if isinstance(status, str) else "<invalid>")

    def report(self, samples, offline):
        """计数输出下界；缺失、回退或不足两个独立快照输出 null。
        Report count lower bounds; missing/regressed/insufficient evidence produces null.
        """
        metrics = {}
        for name, state in self.metrics.items():
            first, last = state["first"], state["last"]
            reason, delta, span = None, None, None
            if state["missing_samples"]:
                reason = "missing_samples"
            elif samples.bad_since.get(name, math.inf) <= self.end:
                reason = "counter_regressed"
            elif first is None or last is None or first is last or first[2] > last[1]:
                reason = "insufficient_independent_samples"
            else:
                delta, span = last[0] - first[0], last[1] - first[2]
            metrics[name] = {"delta": str(delta) if delta is not None and not name.startswith("active_seconds.") else delta,
                             "observed_span_seconds": span, "missing_samples": state["missing_samples"], "unavailable_reason": reason}
        span = max(0, self.last["start"] - self.first["end"]) if self.first else None
        consumers = {}
        for group in GROUPS:
            online = self.end - self.begin - offline[group] if offline[group] is not None else None
            active = metrics["active_seconds." + group]["delta"]
            consumers[group] = {"planned_offline_seconds": offline[group], "planned_online_seconds": online,
                                "observed_active_seconds": active,
                                "active_coverage_ratio": active / online if active is not None and online is not None and online > 0 else None,
                                "sampled_termination_states": dict(self.statuses[group])}
        return {"begin_seconds": self.begin, "end_seconds": self.end, "full_window": self.end - self.begin == WINDOW_SECONDS,
                "contained_samples": self.count, "crossing_samples_excluded": self.crossing,
                "observed_span_seconds": span, "max_sample_gap_seconds": self.max_gap if self.count > 1 else None,
                "max_sample_start_gap_seconds": self.max_start_gap if self.count > 1 else None,
                "max_sample_duration_seconds": self.max_duration,
                "edge_gaps_seconds": {"start": self.first["start"] - self.begin, "end": self.end - self.last["end"]} if self.first else None,
                "metrics": metrics, "offline_timeline_complete": all(value is not None for value in offline.values()),
                "consumers": consumers}


def consumer_events(reader, expected_stream, diagnostics):
    """普通事件只有抽样覆盖，不用样本数估计唯一 ACK。
    Ordinary events are sampled and never used to estimate unique ACK totals.
    """
    counts = Counter()
    for row in reader.rows():
        group = row.get("consumer")
        if (row.get("category") != "event_sample" or group not in GROUPS or type(row.get("event_type")) is not str
                or row["event_type"] not in EVENT_KINDS or type(row.get("database")) is not int
                or row["database"] != int(group[-1]) or type(row.get("shard")) is not int or row["shard"] not in range(4)
                or expected_stream is None
                or unsigned(row.get("sequence")) in (None, 0) or row.get("stream_id") != expected_stream):
            diagnostics.add("invalid_consumer_event_sample")
        else:
            counts[group] += 1
    return {"sample_counts": dict(counts), "scope": "sampled identities only; unique ACKs come from consumer watermarks"}


def analyze_phase(directory, emit, workers=None):
    """单阶段流式分析；emit 即时消费窗口，内存不随长测时长增长。
    Analyze one phase; emit consumes each window immediately, independent of run length.
    """
    diagnostics = Diagnostics()
    result = read_json(directory / "result.json", diagnostics)
    environment = read_json(directory.parent / "environment.json", diagnostics, required=False)
    configured = mapping(environment.get("settings")).get("workers")
    if workers is None:
        workers = configured
    if type(workers) is not int or not 2 <= workers <= 16:
        diagnostics.add("worker_count_missing_or_invalid")
        workers = 0
    elif configured is not None and configured != workers:
        diagnostics.add("worker_count_override_mismatch")
    end = number(result.get("load_stop_requested_elapsed_seconds"))
    if end is None:
        diagnostics.add("load_stop_boundary_missing")
        end = 0
    phase_started = number(result.get("load_started_monotonic_seconds"))
    if phase_started is None:
        diagnostics.add("load_start_monotonic_missing")
    readers = {name: JsonLines(directory / name, diagnostics) for name in
               ("resources.jsonl.gz", "errors.jsonl.gz", "consumer-events.jsonl.gz")}
    warmup = number(mapping(environment.get("settings")).get("warmup_seconds"))
    if warmup is None:
        diagnostics.add("resource_warmup_missing_or_invalid")
    samples = Samples(readers["resources.jsonl.gz"], result, workers, diagnostics, warmup)
    stream = iter(samples.ordered())
    current = next(stream, None)
    offline = OfflineSweep(readers["errors.jsonl.gz"], phase_started, result.get("consumers"), diagnostics)
    minima = {name: {"value": None, "begin_seconds": None, "usable_full_windows": 0} for name in metric_names(workers)}
    full_count, partial_count = 0, 0
    begin = 0
    while begin < end:
        stop = min(begin + WINDOW_SECONDS, end)
        window = Window(begin, stop, metric_names(workers))
        while current is not None and current["start"] <= stop:
            if current["source"] == "terminal" or current["end"] < begin:
                current = next(stream, None)
                continue
            if current["start"] >= begin and current["end"] <= stop:
                window.add(current)
                if current["start"] == current["end"] == stop and stop < end:
                    break
            elif current["start"] < stop and current["end"] > begin:
                window.crossing += 1
                window.max_duration = max(window.max_duration, current["end"] - current["start"])
            if current["end"] > stop:
                break
            current = next(stream, None)
        report = window.report(samples, offline.window(begin, stop))
        if report["full_window"]:
            full_count += 1
            for name, metric in report["metrics"].items():
                if metric["delta"] is None:
                    continue
                value = metric["delta"] if name.startswith("active_seconds.") else int(metric["delta"])
                item = minima[name]
                item["usable_full_windows"] += 1
                if item["value"] is None or value < item["value"]:
                    item["value"], item["begin_seconds"] = value, begin
        else:
            partial_count += 1
        emit(report)
        begin = stop
    for _ in stream:
        pass
    offline_summary = offline.finish()
    hello_stream = wrapped(wrapped(result, "hello"), "stream_id")
    expected_stream = hello_stream.get("hex") if isinstance(hello_stream, dict) else None
    if not isinstance(expected_stream, str) or len(expected_stream) != 32:
        diagnostics.add("hello_stream_id_missing")
    event_summary = consumer_events(readers["consumer-events.jsonl.gz"], expected_stream, diagnostics)
    if readers["consumer-events.jsonl.gz"].stats["rows"] != result.get("consumer_event_samples"):
        diagnostics.add("consumer_event_sample_count_mismatch")
    omitted = unsigned(mapping(result.get("counts")).get("resource_samples_omitted", 0))
    if omitted is None or omitted > 0:
        diagnostics.add("resource_samples_omitted_or_unknown")
    for state in samples.info_counters.values():
        for key in ("first", "last", "max"):
            if state[key] is not None:
                state[key] = str(state[key])
    for name, item in minima.items():
        item["unavailable_full_windows"] = full_count - item["usable_full_windows"]
        if item["value"] is not None and not name.startswith("active_seconds."):
            item["value"] = str(item["value"])
    return {"event_mode": result.get("event_mode"), "phase": result.get("phase"), "policy": result.get("policy"),
            "seed": result.get("seed"), "status": result.get("status"), "workers": workers or None,
            "requested_seconds": result.get("requested_seconds"), "load_stop_requested_elapsed_seconds": end,
            "diagnostic_elapsed_seconds": result.get("elapsed_seconds"), "full_windows": full_count, "partial_windows": partial_count,
            "observed_minima": minima, "resource_rows": samples.resource_rows,
            "max_resource_sample_duration_seconds": samples.max_duration, "max_resource_start_gap_seconds": samples.max_start_gap,
            "journal_sampled_peaks": {key: {**value, "bytes": str(value["bytes"])} for key, value in samples.peaks.items()},
            "resource_trends_recomputed": samples.trends.report(), "resource_trends_recorded": result.get("resource_trends"),
            "native_availability": samples.native_availability,
            "resource_samples_omitted": str(omitted) if omitted is not None else None,
            "info_event_counter_observations": samples.info_counters,
            "offline": offline_summary, "consumer_event_samples": event_summary,
            "recorded_findings": result.get("findings"), "recorded_server_errors": result.get("server_errors"),
            "recorded_consumers": result.get("consumers"), "source_files": {name: reader.stats for name, reader in readers.items()},
            "diagnostics": diagnostics.summary(), "scope": "observations only; no acceptance threshold or pass decision"}


def phase_directories(paths):
    """只展开一层现有阶段目录，不递归猜测任意文件树。
    Expand one level of existing phase directories, not an arbitrary recursive tree.
    """
    for path in paths:
        if (path / "result.json").is_file():
            yield path.resolve()
        else:
            children = sorted(child for child in path.iterdir() if child.is_dir() and (child / "result.json").is_file())
            if not children:
                raise ValueError(f"no phase result.json under {path}")
            yield from (child.resolve() for child in children)


def main(argv=None):
    """窗口直接写 JSON 文件；CLI 不缓存整场报告。
    Write windows directly to JSON without buffering the full report.
    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directories", type=Path, nargs="+")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--workers", type=int, help="explicit expected workers when environment.json is unavailable")
    args = parser.parse_args(argv)
    if args.workers is not None and not 2 <= args.workers <= 16:
        parser.error("--workers must be between 2 and 16")
    try:
        directories = list(phase_directories(args.directories))
    except (OSError, ValueError) as error:
        parser.error(str(error))
    if args.output.resolve() in {directory / "result.json" for directory in directories}:
        parser.error("output must not overwrite input metadata")
    with args.output.open("x", encoding="utf-8") as output:
        output.write('{"format":"ling-v030-observations-1","window_seconds":60,"phases":[')
        for index, directory in enumerate(directories):
            if index:
                output.write(",")
            output.write('{"directory":' + json.dumps(str(directory)) + ',"windows":[')
            first = True
            def emit(row):
                """即时序列化一个窗口；Serialize one window immediately."""
                nonlocal first
                if not first:
                    output.write(",")
                json.dump(row, output, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
                first = False
            summary = analyze_phase(directory, emit, args.workers)
            output.write('],"summary":')
            json.dump(summary, output, ensure_ascii=False, allow_nan=False, separators=(",", ":"))
            output.write("}")
        output.write("]}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
