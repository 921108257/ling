"""逐案例归档和独立回读开放负载证据；Archive and reread the six completed open-load cases."""
from collections import Counter
from datetime import datetime, timezone
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "target/open-full-20261118-0420"
DESTINATION = ROOT / "benchmarks/results/v030-open-2026-11-18"
CASES = [f"{mode}-{repeat}" for mode in ("fast", "reliable") for repeat in range(3)]


def sha256(path):
    """不整件载入内存；Hash files using bounded memory."""
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read(path):
    """证据 JSON 均明确使用 UTF-8；Read evidence JSON explicitly as UTF-8."""
    return json.loads(path.read_text(encoding="utf-8"))


def save(path, data):
    """仅写新归档目录；Write only the newly created archive directory."""
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def resource_summary(path):
    """保留真实采样间隔与缺失，不推断连续峰值；Retain actual gaps and missing samples, not continuous peaks."""
    rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
    stamps = [row["monotonic_ns"] for row in rows]
    gaps = [(end - start) / 1e9 for start, end in zip(stamps, stamps[1:])]
    result = {"samples": len(rows), "first_monotonic_ns": stamps[0], "last_monotonic_ns": stamps[-1],
              "sample_span_seconds": (stamps[-1] - stamps[0]) / 1e9,
              "actual_gap_seconds": {"minimum": min(gaps), "maximum": max(gaps), "mean": sum(gaps) / len(gaps)},
              "info_error_samples": sum("info_error" in row for row in rows),
              "info_error_counts": dict(Counter(str(row["info_error"]) for row in rows if "info_error" in row))}
    for subject in ("server", "driver"):
        fields = {}
        for field in ("cpu_seconds", "rss_bytes", "peak_rss_bytes", "handles", "threads"):
            values = [row.get(subject, {}).get(field) for row in rows]
            valid = [(number, value) for number, value in enumerate(values) if type(value) in (int, float)]
            fields[field] = {"valid_samples": len(valid), "missing_samples": len(rows) - len(valid),
                             "first": valid[0][1] if valid else None, "last": valid[-1][1] if valid else None,
                             "minimum": min(value for _, value in valid) if valid else None,
                             "maximum_observed": max(value for _, value in valid) if valid else None}
        result[subject] = fields
    native_errors = Counter(str(row.get(subject + "_error")) for row in rows for subject in ("server", "driver")
                            if subject + "_error" in row)
    result["native_error_counts"] = dict(native_errors)
    last_info = next((row["info"] for row in reversed(rows) if isinstance(row.get("info"), dict)), None)
    result["last_available_info"] = ({"stats": last_info.get("stats"), "events": last_info.get("events"),
                                      "memory": last_info.get("memory"), "clients": last_info.get("clients")}
                                     if last_info else None)
    return result


def main():
    """所有归档逐成员回读、逐字节验证；Verify every archive member and extracted byte."""
    if DESTINATION.exists():
        raise RuntimeError("Archive destination already exists; do not overwrite evidence")
    audit_path = ROOT / "target/open-full-20261118-0420-audit.json"
    audit, plan, original = read(audit_path), read(SOURCE / "plan.json"), read(SOURCE / "summary.json")
    if set(path.name for path in SOURCE.iterdir() if path.is_dir()) != set(CASES):
        raise RuntimeError("Unexpected case directories")
    analyzer = ROOT / "target/analyze-open-results.py"
    if audit["analyzer_sha256"] != sha256(analyzer):
        raise RuntimeError("Audit analyzer identity mismatch")
    DESTINATION.mkdir()
    for source, name in ((audit_path, "audit.json"), (analyzer, analyzer.name),
                         (ROOT / "target/test_analyze_open_results.py", "test_analyze_open_results.py"),
                         (Path(__file__), "archive-open-evidence.py")):
        shutil.copyfile(source, DESTINATION / name)
    # 保存独立测试的真实命令、环境和原始输出；Retain the exact offline command, environment and raw output.
    command = [sys.executable, "-B", "-m", "unittest", "discover", "-s", str(DESTINATION),
               "-p", "test_analyze_open_results.py", "-v"]
    environment = dict(os.environ, PYTHONIOENCODING="utf-8", PYTHONDONTWRITEBYTECODE="1")
    with (DESTINATION / "offline-tests.stdout.bin").open("xb") as stdout, (
            DESTINATION / "offline-tests.stderr.bin").open("xb") as stderr:
        test = subprocess.run(command, cwd=ROOT, stdout=stdout, stderr=stderr, env=environment, check=False)
    save(DESTINATION / "offline-tests.json", {"command": command, "cwd": str(ROOT), "exit_code": test.returncode,
         "python": sys.version, "platform": platform.platform(),
         "scope": "offline analyzer fixtures; no server, WSL, network or workload was started"})
    if test.returncode:
        raise RuntimeError("Offline analyzer regression failed")

    files = []
    for path in sorted(SOURCE.rglob("*")):
        if path.is_symlink():
            raise RuntimeError(f"Symlink not allowed: {path}")
        if path.is_file():
            stat = path.stat()
            relative = path.relative_to(SOURCE).as_posix()
            group = relative.split("/")[0] if "/" in relative else "root"
            if group not in CASES + ["root"]:
                raise RuntimeError(f"Unexpected source: {relative}")
            files.append({"path": relative, "archive": "raw-" + group + ".tar.gz", "bytes": stat.st_size,
                          "sha256": sha256(path), "source_mtime_ns": stat.st_mtime_ns})
    # 交叉核验本地原件与审计器和每轮固定输入身份；Cross-check originals with audit and per-run frozen inputs.
    by_path = {row["path"]: row for row in files}
    for case, row in zip(CASES, audit["rounds"]):
        raw = by_path[case + "/load/requests.jsonl"]
        if raw["sha256"] != row["raw"]["sha256"] or raw["bytes"] != row["raw"]["bytes"]:
            raise RuntimeError(f"Audit raw identity mismatch: {case}")
        inputs = read(SOURCE / case / "load/plan.json")["inputs"]
        for item in inputs.values():
            if by_path[case + "/load/" + item["copy"]]["sha256"] != item["sha256"]:
                raise RuntimeError(f"Frozen input mismatch: {case}/{item['copy']}")
    if by_path["ling"]["sha256"] != plan["release_sha256"]:
        raise RuntimeError("Release binary mismatch")
    save(DESTINATION / "evidence-files.manifest.json", {"source_root": str(SOURCE), "files": files})
    validations = []
    for group in ["root"] + CASES:
        archive = DESTINATION / ("raw-" + group + ".tar.gz")
        selected = [row for row in files if row["archive"] == archive.name]
        with tarfile.open(archive, "w:gz", compresslevel=6) as bundle:
            for row in selected:
                bundle.add(SOURCE / row["path"], arcname=row["path"], recursive=False)
        if archive.stat().st_size >= 90 * 1024 * 1024:
            raise RuntimeError(f"Archive exceeds planned Git file budget: {archive.name}")
        expected = {row["path"]: row for row in selected}
        # 完整读至 gzip EOF；显式校验路径，手动提取到临时目录后重新计算摘要。
        # Read through gzip EOF, validate paths, then hash a temporary extraction independently.
        with tempfile.TemporaryDirectory(prefix="ling-open-archive-") as temporary:
            extracted = Path(temporary)
            seen = set()
            with tarfile.open(archive, "r:gz") as bundle:
                for member in bundle:
                    parts = PurePosixPath(member.name)
                    if not member.isfile() or parts.is_absolute() or ".." in parts.parts or member.name not in expected or member.name in seen:
                        raise RuntimeError(f"Invalid archive member: {member.name}")
                    row = expected[member.name]
                    if member.size != row["bytes"]:
                        raise RuntimeError(f"Archive size mismatch: {member.name}")
                    destination = extracted.joinpath(*parts.parts)
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    with bundle.extractfile(member) as stream, destination.open("xb") as target:
                        shutil.copyfileobj(stream, target, 1024 * 1024)
                    if sha256(destination) != row["sha256"]:
                        raise RuntimeError(f"Extracted SHA mismatch: {member.name}")
                    seen.add(member.name)
            if seen != set(expected):
                raise RuntimeError("Archive member set differs")
        uncompressed_bytes = 0
        with gzip.open(archive, "rb") as stream:
            while block := stream.read(1024 * 1024):
                uncompressed_bytes += len(block)
        # 源文件须在整个归档阶段保持不变；Reject source changes during the archive operation.
        for row in selected:
            source = SOURCE / row["path"]
            if source.stat().st_size != row["bytes"] or sha256(source) != row["sha256"]:
                raise RuntimeError(f"Source changed during archive: {row['path']}")
        validations.append({"path": archive.name, "members": len(seen), "original_file_bytes": sum(row["bytes"] for row in selected),
                            "gzip_uncompressed_tar_bytes": uncompressed_bytes,
                            "duplicate_or_unsafe_members": 0, "extracted_size_sha256_matches": True,
                            "gzip_read_to_eof": True, "source_unchanged": True})
        print(json.dumps({"archive": archive.name, "bytes": archive.stat().st_size, "files_verified": len(seen)}), flush=True)

    summary = {"run_id": SOURCE.name, "stage": "pre-release", "binary": {"version": plan["release_version"],
               "revision": plan["release_revision"], "sha256": plan["release_sha256"]},
               "tool_source": {"revision": plan["source_head"], "status": plan["source_status"]},
               "plan": plan, "original_passed": original["passed"],
               "independent_evidence_consistent": audit["evidence_consistent"], "audit_diagnostics": audit["diagnostics"],
               "accounting_semantics": audit["semantics"], "cases": []}
    total = Counter()
    total_errors, total_outcomes = Counter(), Counter()
    for case, audited, original_case in zip(CASES, audit["rounds"], original["results"]):
        actual = read(SOURCE / case / "result.json")
        if actual != original_case:
            raise RuntimeError(f"Root and case result differ: {case}")
        phases = {}
        for name, phase in audited["phases"].items():
            counts = phase["totals"]
            total.update({key: value for key, value in counts.items() if type(value) is int})
            total_errors.update(counts["error_codes"])
            total_outcomes.update(counts["outcomes"])
            phases[name] = {"planned_scheduled": phase["planned_scheduled"], "totals": counts,
                            "by_command": phase["by_command"], "aggregate_accounting": phase["aggregate_accounting"],
                            "identity_anomalies": phase["identity_anomalies"],
                            "summary_count_differences": phase["summary_count_differences"],
                            "summary_error_differences": phase["summary_error_differences"],
                            "reported_latencies": actual["load_summary"]["phases"][name]["latencies"]}
        resources = resource_summary(SOURCE / case / "resources.jsonl")
        resources["last_sample_before_recovery_confirmation_seconds"] = (
            actual["load_summary"]["recovery"]["confirmed_ns"] - resources["last_monotonic_ns"]) / 1e9
        summary["cases"].append({"name": case, "mode": audited["mode"], "repeat": audited["repeat"],
            "passed": actual["passed"], "driver_exit": actual["driver_exit"], "server_exit": actual["server_exit"],
            "shutdown_response": actual["shutdown_response"], "cleanup_errors": actual["cleanup_errors"],
            "evidence_consistent": audited["evidence_consistent"], "diagnostics": audited["diagnostics"],
            "raw_records": audited["raw"], "request_sequence": audited["request_sequence"],
            "recovery": actual["load_summary"]["recovery"], "maximum_queued": actual["load_summary"]["maximum_queued"],
            "phases": phases, "resources": resources})
    summary["totals"] = {**dict(total), "error_codes": dict(total_errors), "outcomes": dict(total_outcomes)}
    summary["limitations"] = [
        "Original valid/passed describes complete tool accounting and final PING recovery; it does not mean zero request failures or production acceptance.",
        "Successful completions, tester-dropped arrivals, queued cancellations and unknown SET outcomes remain separate; an unknown result proves neither commitment nor loss.",
        "Baseline, overload and recovery labels refer to scheduled phases. Planned 5000 requests/s is not inferred achieved throughput or proof of server saturation.",
        "This was one shared WSL host, Python client, 32 serial connections and a bounded queue; client CPU/GIL, logging and scheduling may limit generated load.",
        "The recovery confirmation begins after the 60-second recovery phase and queue drain; it is not the first instant of service recovery after overload.",
        "Resource samples use actual monotonic timestamps. One-second sleep plus INFO time is not a strict one-second sampling interval; missing INFO remains missing.",
        "Each sample timestamps before reading native resources and INFO, without a completion-time bracket. CPU seconds is cumulative CPU time; handles is Linux /proc/PID/fd count; peak_rss_bytes is the separately observed /proc VmHWM value.",
        "Resource maxima are observed samples only. No terminal resource sample follows driver exit, so this does not prove peak bounds, a stable plateau, leak absence or three-day behavior.",
        "Every last resource sample precedes final recovery confirmation. INFO stats/events timing_enabled is false; zero INFO histograms are not latency distributions.",
        "Fast event drops are visible in the resource INFO records; this test has no subscribers and does not claim event-delivery reliability.",
        "The archived Windows copy does not establish the original Linux file owner or mode metadata.",
        "No physical power failure, release 0.3.0 binary or post-release test was run in this evidence set."]
    save(DESTINATION / "summary.json", summary)
    save(DESTINATION / "archive-verification.json", {"method": "per-file SHA256 and byte size before archiving; safe temporary extraction and independent rehash; full gzip EOF; source rehash after archive", "archives": validations})
    manifest_files = [{"path": path.name, "bytes": path.stat().st_size, "sha256": sha256(path)}
                      for path in sorted(DESTINATION.iterdir()) if path.is_file()]
    save(DESTINATION / "manifest.json", {"archived_utc": datetime.now(timezone.utc).isoformat(), "run_id": SOURCE.name,
         "release_revision": plan["release_revision"], "release_sha256": plan["release_sha256"],
         "source_file_count": len(files), "source_file_bytes": sum(row["bytes"] for row in files),
         "archive_count": len(validations), "scope": "pre-release open-load evidence and independent accounting; retains request errors and unknown outcomes", "files": manifest_files})
    for row in manifest_files:
        path = DESTINATION / row["path"]
        if path.stat().st_size != row["bytes"] or sha256(path) != row["sha256"]:
            raise RuntimeError(f"Final manifest mismatch: {path}")
    print(json.dumps({"destination": str(DESTINATION), "files": len(files), "archive_count": len(validations),
                      "manifest_files": len(manifest_files), "totals": summary["totals"]}), flush=True)


if __name__ == "__main__":
    main()
