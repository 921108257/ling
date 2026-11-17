#!/usr/bin/env python3
"""独立只读核验正式 Redis 对照证据；Independently audit the complete 72-case comparison."""
import argparse
from array import array
from collections import Counter
import gzip
import hashlib
import json
import math
from pathlib import Path
import re
import statistics
import zlib

MAX_SAMPLES = 2_000_000
MAX_RAW_BYTES = 64 * 1024 * 1024
IDENTITY = ("mode", "kind", "operation", "repeat", "system")
GROUP = ("mode", "kind", "operation", "system")
METRICS = ("successes_per_second", "p50_us", "p95_us", "p99_us", "p999_us",
           "server_cpu_seconds", "client_cpu_seconds")
FORMAL_ARGUMENTS = {"seconds": 10, "repeats": 3, "connections": 4, "keys": 1000,
                    "value_bytes": 64, "mode": "both", "kind": "both", "operation": "all"}
# 本轮已独立核对的公开制品与 Redis 构建，不从被核验证据读取预期值。
# Independently supplied pins for this acceptance run; never derive expectations from its manifest.
EXPECTED_IDENTITY = {
    "ling_sha256": "70b209554df2f236c6c4c30d6bbae5ec001a3b2754761038265483f659a0f285",
    "ling_revision": "441ffc7dbb2bf48d6257c0ca3b831f099554af1a",
    "ling_version": "ling 0.2.0",
    "redis_sha256": "1ea6a05a0108e60c7a4656592138714da2c21888f72b2937301eed487f6b778a",
    "redis_version": "8.10.3",
    "redis_source_sha256": "edde778f6bdee7581e42fc139ca9e07e2822212092f7501f99989656e3629d02",
}


def sha256(path):
    """流式摘要，不执行受测输入；Hash files without executing any evidence input."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def unique_object(pairs):
    """拒绝重复JSON键，避免后值覆盖证据；Reject duplicate keys instead of hiding evidence."""
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key: " + key)
        result[key] = value
    return result


def read_json(path):
    """读取小型元数据并拒绝非有限JSON数字；Read bounded, strict metadata JSON."""
    if path.stat().st_size > MAX_RAW_BYTES:
        raise ValueError("metadata exceeds byte budget")
    def invalid(value):
        raise ValueError("non-finite JSON number: " + value)
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object,
                      parse_constant=invalid)


def read_latencies(path, limit=MAX_SAMPLES):
    """流式验证gzip和整数数组，单轮样本有硬上限；Validate gzip and a bounded unsigned-ns array."""
    values, token, state, total = array("Q"), "", "start", 0
    with gzip.open(path, "rt", encoding="ascii") as source:
        while chunk := source.read(65536):
            total += len(chunk)
            if total > MAX_RAW_BYTES:
                raise ValueError("raw latency byte budget exceeded")
            for character in chunk:
                if state == "number" and not character.isdecimal():
                    value = int(token)
                    if value > 2**64 - 1 or len(values) >= limit:
                        raise ValueError("latency integer or sample budget exceeded")
                    values.append(value)
                    token, state = "", "separator"
                if character in " \t\r\n":
                    continue
                if state == "start" and character == "[":
                    state = "first"
                elif state in ("first", "value") and "0" <= character <= "9":
                    token, state = character, "number"
                elif state == "number" and "0" <= character <= "9":
                    if token == "0" or len(token) >= 20:
                        raise ValueError("invalid latency integer")
                    token += character
                elif state == "separator" and character == ",":
                    state = "value"
                elif state in ("first", "separator") and character == "]":
                    state = "done"
                else:
                    raise ValueError("invalid latency JSON array")
    if state != "done":
        raise ValueError("incomplete latency JSON array")
    return values


def identity(row, fields=IDENTITY):
    """固定场景身份不依赖目录迭代顺序；Use the fixed case identity, never directory order."""
    return tuple(row[field] for field in fields)


def finite(value):
    """Bool不是可接受的测量数字；Booleans are not measurement numbers."""
    return type(value) in (int, float) and math.isfinite(value)


def same_number(actual, expected):
    """仅容忍JSON浮点回读误差；Tolerate only floating-point round-trip noise."""
    return finite(actual) and math.isclose(actual, expected, rel_tol=1e-12, abs_tol=1e-9)


def audit_case(root, row, seconds):
    """独立重算一轮，保留所有失败原因；Recompute one repeat and retain each failed check."""
    name = "-".join(str(row[field]) for field in IDENTITY)
    directory, errors = root / name, []
    report = {"name": name, "errors": errors}
    def check(condition, reason):
        if not condition:
            errors.append(reason)
    try:
        directory = directory.resolve(strict=True)
        if directory.parent != root:
            raise ValueError("case directory escapes run root")
        for filename in ("result.json", "latencies-ns.json.gz"):
            if not (directory / filename).resolve(strict=True).is_relative_to(directory):
                raise ValueError("case file escapes its directory")
        check(read_json(directory / "result.json") == row, "per-case result differs from aggregate")
        check(row.get("valid") is True, "case valid is not true")
        check(row.get("errors") == [], "case has errors or missing error list")
        check(type(row.get("threads_alive")) is int and row["threads_alive"] == 0,
              "case has live workers or missing worker status")
        check(type(row.get("pipeline")) is int and row["pipeline"] == 1
              and type(row.get("connections")) is int and row["connections"] == 4,
              "connection/pipeline mismatch")
        raw = directory / "latencies-ns.json.gz"
        compressed_digest = sha256(raw)
        samples = read_latencies(raw)
        check(sha256(raw) == compressed_digest, "raw latency file changed during audit")
        report.update(raw_sha256=compressed_digest, raw_samples=len(samples))
        check(bool(samples), "empty raw latency samples")
        counts = row.get("worker_counts")
        valid_counts = (isinstance(counts, list) and len(counts) == 4
                        and all(type(value) is int and 0 < value < MAX_SAMPLES // 4 for value in counts))
        check(valid_counts, "worker sample counts invalid or budget exhausted")
        check(type(row.get("successes")) is int and row["successes"] == len(samples), "success count differs from raw samples")
        check(valid_counts and sum(counts) == len(samples), "worker counts differ from raw samples")
        started, planned, ended = (row.get(field) for field in ("start_ns", "planned_end_ns", "measured_end_ns"))
        valid_times = (all(type(value) is int for value in (started, planned, ended))
                       and 0 <= started < planned <= ended
                       and planned - started == round(seconds * 1e9))
        check(valid_times, "measurement timestamps or requested duration invalid")
        if valid_times:
            elapsed = (ended - started) / 1e9
            rate = len(samples) / elapsed
            report.update(elapsed_seconds=elapsed, successes_per_second=rate)
            check(same_number(row.get("elapsed_seconds"), elapsed), "elapsed seconds mismatch")
            check(same_number(row.get("successes_per_second"), rate), "throughput mismatch")
            check(not samples or max(samples) <= ended - started, "raw latency exceeds measurement span")
        if samples:
            ordered = sorted(samples)
            for field, numerator in (("p50_us", 500), ("p95_us", 950), ("p99_us", 990), ("p999_us", 999)):
                rank = (len(ordered) * numerator + 999) // 1000
                expected = ordered[rank - 1] / 1000
                report[field] = expected
                check(same_number(row.get(field), expected), field + " differs from raw nearest rank")
        before, after = row.get("resources_before", {}), row.get("resources_after", {})
        first, last = before.get("cpu_seconds"), after.get("cpu_seconds")
        if finite(first) and finite(last) and 0 <= first <= last:
            report["server_cpu_seconds"] = last - first
            check(same_number(row.get("server_cpu_seconds"), last - first), "server CPU endpoint delta mismatch")
        else:
            errors.append("server CPU endpoints missing or invalid")
        for label, snapshot in (("before", before), ("after", after)):
            rss, peak = snapshot.get("rss_bytes"), snapshot.get("peak_rss_bytes")
            check(type(rss) is int and type(peak) is int and 0 < rss <= peak,
                  label + " RSS / lifetime peak invalid or unavailable")
        if type(before.get("peak_rss_bytes")) is int and type(after.get("peak_rss_bytes")) is int:
            check(before["peak_rss_bytes"] <= after["peak_rss_bytes"], "lifetime RSS peak decreased")
        check(finite(row.get("client_cpu_seconds")) and row["client_cpu_seconds"] >= 0, "client CPU invalid")
        report["client_cpu_seconds_recorded"] = row.get("client_cpu_seconds")
        report["resources_before"], report["resources_after"] = before, after
        report["memory_before"], report["memory_after"] = row.get("memory_before"), row.get("memory_after")
    except (OSError, EOFError, ValueError, KeyError, TypeError, OverflowError, AttributeError, zlib.error) as error:
        errors.append("unreadable or malformed case evidence: " + str(error))
    report["valid"] = not errors
    return report


def audit(root):
    """只读完整正式矩阵，不把部分结果升级为通过；Audit the full formal matrix, never partial success."""
    root = Path(root).resolve(strict=True)
    errors, cases, inputs, calculated_groups = [], [], {}, []
    report = {"root": str(root), "valid": False, "errors": errors, "inputs": inputs,
              "cases": cases, "groups": calculated_groups,
              "expected_identity": dict(EXPECTED_IDENTITY),
              "limits": {"max_samples_per_case": MAX_SAMPLES, "max_raw_bytes_per_case": MAX_RAW_BYTES},
              "scope": "Raw successful request latencies only; client CPU has no recorded endpoints to independently recompute. RSS endpoints and lifetime VmHWM include preload/warmup. Frozen input bytes match independently supplied pins; recorded revision/version metadata must agree, but archive hash alone does not prove binary build provenance. No executable is run."}
    def check(condition, reason):
        if not condition:
            errors.append(reason)
    try:
        environment = read_json(root / "environment.json")
        schedule = read_json(root / "schedule.json")
        rows, summary = read_json(root / "results.json"), read_json(root / "summary.json")
        arguments = environment["arguments"]
        check(all(arguments.get(key) == value for key, value in FORMAL_ARGUMENTS.items()), "formal run arguments mismatch")
        check(environment.get("pipeline") == 1 and environment.get("consumers") == 0, "environment pipeline / consumers mismatch")
        expected = {(mode, kind, operation, repeat, system)
                    for mode in ("fast", "reliable") for kind in ("string", "bytes")
                    for operation in ("get", "set", "set-px") for repeat in (1, 2, 3)
                    for system in ("ling", "redis")}
        for label, values in (("schedule", schedule), ("results", rows)):
            check(all(type(row.get("repeat")) is int for row in values), label + " repeat must be an integer")
            ids = [identity(row) for row in values]
            check(len(ids) == 72 and len(set(ids)) == 72 and set(ids) == expected,
                  label + " does not contain the exact 72-case matrix")
        check([identity(row) for row in rows] == [identity(row) for row in schedule], "result order differs from schedule")
        check(len(schedule) % 2 == 0 and all(
            identity(left)[:4] == identity(right)[:4] and {left["system"], right["system"]} == {"ling", "redis"}
            for left, right in zip(schedule[::2], schedule[1::2])), "schedule is not adjacent interleaved system pairs")
        check(summary.get("valid") is True and summary.get("planned_cases") == 72
              and summary.get("executed_cases") == 72, "summary is incomplete or invalid")
        check(summary.get("results") == rows, "summary results differ from results.json")
        source_inputs = environment["inputs"]
        check({"controller", "protocol_helper", "tests", "ling", "redis", "redis_source_archive"} <= set(source_inputs),
              "required frozen inputs missing")
        for label, record in source_inputs.items():
            try:
                path = (root / record["copy"]).resolve(strict=True)
                if not path.is_relative_to(root / "inputs") or not path.is_file():
                    raise ValueError("input path escapes inputs directory or is not a file")
                actual = sha256(path)
                inputs[label] = {"copy": record["copy"], "sha256": actual, "valid": actual == record.get("sha256")}
                check(inputs[label]["valid"], "input SHA-256 mismatch: " + label)
            except (OSError, ValueError, KeyError, TypeError) as error:
                errors.append("invalid frozen input " + label + ": " + str(error))
        # 这里比较重新计算的文件摘要，而非再次信任 manifest 中的同一个字符串。
        # Bind actual copied bytes to external pins, independently of manifest consistency.
        identity_checks = {}
        for label, expected_key in (("ling", "ling_sha256"), ("redis", "redis_sha256"),
                                    ("redis_source_archive", "redis_source_sha256")):
            identity_checks[label + "_sha256"] = inputs.get(label, {}).get("sha256") == EXPECTED_IDENTITY[expected_key]
        identity_checks["ling_revision"] = environment.get("ling_revision_operator_asserted") == EXPECTED_IDENTITY["ling_revision"]
        pin = environment.get("redis_pin", {})
        identity_checks["redis_pin"] = (pin.get("version") == EXPECTED_IDENTITY["redis_version"]
                                        and pin.get("sha256") == EXPECTED_IDENTITY["redis_source_sha256"])
        identity_checks["redis_source_archive_verified"] = environment.get("redis_source_archive_verified") is True
        versions = environment.get("binary_versions", {})
        for system in ("ling", "redis"):
            version = versions.get(system, {})
            output = version.get("stdout")
            version_matches = False
            if isinstance(output, str):
                version_matches = (output.strip() == EXPECTED_IDENTITY["ling_version"] if system == "ling" else
                    re.search(r"\bRedis server v=" + re.escape(EXPECTED_IDENTITY["redis_version"])
                              + r"(?:\s|$)", output) is not None)
            identity_checks[system + "_version"] = (type(version.get("returncode")) is int
                                                     and version["returncode"] == 0 and version_matches)
        report["identity_checks"] = identity_checks
        for field, matched in identity_checks.items():
            check(matched, "expected input identity mismatch: " + field)
        for row in rows:
            cases.append(audit_case(root, row, FORMAL_ARGUMENTS["seconds"]))
        check(all(case["valid"] for case in cases), "one or more case audits failed")
        groups = {}
        for row in rows:
            groups.setdefault(identity(row, GROUP), []).append(row)
        check(len(groups) == 24, "expected 24 separate system/scenario groups")
        for key, values in sorted(groups.items()):
            repeats = Counter(row["repeat"] for row in values)
            check(repeats == Counter({1: 1, 2: 1, 3: 1}), "group repeat identities incomplete or duplicated: " + str(key))
            ranges = {}
            for metric in METRICS:
                numbers = [row[metric] for row in values if finite(row.get(metric))]
                check(len(numbers) == 3, "group metric missing: " + str(key) + "/" + metric)
                ranges[metric] = ({"min": min(numbers), "median": statistics.median(numbers), "max": max(numbers)}
                                  if numbers else None)
            calculated_groups.append(dict(zip(GROUP, key)) | {"repeats": len(values),
                "all_valid": all(row.get("valid") is True for row in values), "ranges": ranges})
        check(summary.get("groups") == calculated_groups, "summary group ranges differ from independent aggregation")
        report["case_count"], report["group_count"] = len(cases), len(groups)
    except (OSError, ValueError, KeyError, TypeError, OverflowError, AttributeError) as error:
        errors.append("unreadable or malformed run evidence: " + str(error))
    report["valid"] = not errors
    return report


def main():
    """JSON报告仅写标准输出；Write the audit report to stdout, leaving evidence untouched."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    args = parser.parse_args()
    try:
        report = audit(args.root)
    except OSError as error:
        report = {"valid": False, "errors": [str(error)]}
    print(json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False))
    return 0 if report["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
