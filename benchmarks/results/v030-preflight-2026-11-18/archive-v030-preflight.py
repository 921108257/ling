"""归档预检失败和布局烟测，不启动负载；Archive preflight failure and layout smoke without running workloads."""
from datetime import datetime, timezone
import gzip
import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "benchmarks/results/v030-preflight-2026-11-18"
REDIS = ROOT / "target/redis-full-20261118-0450"
LAYOUT = ROOT / "target/layout-smoke-20261118-0454"


def sha256(path):
    """流式保留原件摘要；Hash original bytes using bounded memory."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def read(path):
    """明确 UTF-8 解码结构化文件；Decode structured files explicitly as UTF-8."""
    return json.loads(path.read_text(encoding="utf-8"))


def save(path, value):
    """只写全新证据目录；Write only the fresh evidence destination."""
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def archive(name, source):
    """原始树、控制日志与退出文件一起保留；Preserve each raw tree with its controller log and exit code."""
    archive_name = name + ".tar.gz"
    sources = []
    for path in sorted(source.rglob("*")):
        if path.is_symlink():
            raise RuntimeError(f"Symlink not allowed: {path}")
        if path.is_file():
            sources.append((path, "raw/" + path.relative_to(source).as_posix()))
    for suffix in ("controller.log", "exit.txt"):
        path = source.with_name(source.name + "-" + suffix)
        sources.append((path, "execution/" + path.name))
    if source == LAYOUT:
        path = ROOT / "target/smoke-v030-layout.py"
        if sha256(path) != read(source / "smoke-profile.json")["harness_sha256"]:
            raise RuntimeError("Smoke harness digest mismatch")
        sources.append((path, "execution/smoke-v030-layout.py"))
    files = [{"archive": archive_name, "path": name_in_archive, "source_path": str(path.relative_to(ROOT)),
              "bytes": path.stat().st_size, "sha256": sha256(path), "source_mtime_ns": path.stat().st_mtime_ns}
             for path, name_in_archive in sources]
    path = OUT / archive_name
    with tarfile.open(path, "w:gz", compresslevel=6) as bundle:
        for source_path, name_in_archive in sources:
            bundle.add(source_path, arcname=name_in_archive, recursive=False)
    if path.stat().st_size >= 90 * 1024 * 1024:
        raise RuntimeError("Archive exceeds the 90 MiB limit")
    expected = {row["path"]: row for row in files}
    seen, nested_gzip_count = set(), 0
    with tempfile.TemporaryDirectory(prefix="ling-preflight-verify-") as temporary:
        with tarfile.open(path, "r:gz") as bundle:
            for member in bundle:
                relative = PurePosixPath(member.name)
                if (not member.isfile() or relative.is_absolute() or ".." in relative.parts
                        or member.name not in expected or member.name in seen):
                    raise RuntimeError(f"Unsafe or duplicate archive member: {member.name}")
                target = Path(temporary).joinpath(*relative.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                with bundle.extractfile(member) as source_stream, target.open("xb") as sink:
                    shutil.copyfileobj(source_stream, sink, 1024 * 1024)
                if target.stat().st_size != expected[member.name]["bytes"] or sha256(target) != expected[member.name]["sha256"]:
                    raise RuntimeError(f"Extracted bytes differ: {member.name}")
                if target.suffix == ".gz":
                    with gzip.open(target, "rb") as stream:
                        while stream.read(1024 * 1024):
                            pass
                    nested_gzip_count += 1
                seen.add(member.name)
    if seen != set(expected):
        raise RuntimeError("Archive member set differs")
    with gzip.open(path, "rb") as stream:
        while stream.read(1024 * 1024):
            pass
    for (source_path, _), row in zip(sources, files):
        if source_path.stat().st_size != row["bytes"] or sha256(source_path) != row["sha256"]:
            raise RuntimeError(f"Source changed: {source_path}")
    verification = {"archive": archive_name, "members": len(seen), "source_bytes": sum(row["bytes"] for row in files),
                    "extracted_sha256_size_match": True, "gzip_read_to_eof": True,
                    "nested_gzip_read_to_eof": nested_gzip_count, "source_unchanged": True}
    return files, verification


def main():
    """验证实际失败与短测范围，保存独立摘要；Verify the failure and smoke scope before summarizing."""
    if OUT.exists():
        raise RuntimeError("Refuse to overwrite an existing archive")
    OUT.mkdir()
    (OUT / ".gitattributes").write_text("* -text whitespace=cr-at-eol\n*.gz -diff\n*.bin -diff\n", encoding="utf-8")
    shutil.copyfile(__file__, OUT / "archive-v030-preflight.py")
    redis_summary, results, environment = read(REDIS / "summary.json"), read(REDIS / "results.json"), read(REDIS / "environment.json")
    if redis_summary["valid"] is not False or redis_summary["planned_cases"] != 72 or len(results) != 1:
        raise RuntimeError("Unexpected first-failure scope")
    result = results[0]
    if result["valid"] is not False or result["system"] != "ling" or result["operation"] != "set-px":
        raise RuntimeError("Unexpected failed case")
    if any(key in result for key in ("successes_per_second", "p99_us")):
        raise RuntimeError("First failure unexpectedly contains a valid measured result")
    for item in environment["inputs"].values():
        if sha256(REDIS / item["copy"]) != item["sha256"]:
            raise RuntimeError(f"Redis input identity mismatch: {item['copy']}")
    layout, profile, provenance = read(LAYOUT / "layout-result.json"), read(LAYOUT / "smoke-profile.json"), read(LAYOUT / "layout.json")
    phases = []
    for path in sorted(LAYOUT.rglob("result.json")):
        row = read(path)
        required = ["no_findings", "full_duration", "all_workers_completed", "consumers_valid", "native_resources_available"]
        if row["phase"] == "shadow":
            required += ["all_ten_types_checked", "shadow_writes_not_capacity_limited"]
        if row["status"] != "duration_reached" or row["server_exit_code"] != 0 or row["findings"] or any(row["criteria"].get(key) is not True for key in required):
            raise RuntimeError(f"Unexpected smoke phase failure: {path}")
        phases.append({"name": path.parent.name, "phase": row["phase"], "requested_seconds": row["requested_seconds"],
                       "actual_load_seconds": row["load_stop_requested_elapsed_seconds"],
                       "load_start_monotonic_seconds": row["load_started_monotonic_seconds"],
                       "status": row["status"], "server_exit_code": row["server_exit_code"],
                       "findings": row["findings"], "criteria": row["criteria"]})
    shadows = [row for row in phases if row["phase"] == "shadow"]
    pressures = [row for row in phases if row["phase"] == "pressure"]
    if len(shadows) != 2 or len(pressures) != 10 or profile["shadow_seconds"] != 90 or profile["pressure_seconds_each"] != 10:
        raise RuntimeError("Unexpected smoke duration or phase count")
    common = {"start": max(row["load_start_monotonic_seconds"] for row in shadows),
              "end": min(row["load_start_monotonic_seconds"] + row["actual_load_seconds"] for row in shadows)}
    if any(not common["start"] <= row["load_start_monotonic_seconds"] <= row["load_start_monotonic_seconds"] + row["actual_load_seconds"] <= common["end"] for row in pressures):
        raise RuntimeError("Pressure outside actual shadow overlap")
    if any(row["exit_code"] != 0 for row in layout["driver_exits"]) or layout["remaining_processes"]:
        raise RuntimeError("Unexpected driver exit or process remainder")
    for source_path, digest in provenance["script_and_tool_sha256"].items():
        if sha256(LAYOUT / "tool_source" / Path(source_path).name) != digest:
            raise RuntimeError("Layout frozen tool digest mismatch")
    failure_summary = {"run_id": REDIS.name, "controller_exit": int(REDIS.with_name(REDIS.name + "-exit.txt").read_text()),
        "valid": False, "planned_cases": 72, "executed_cases": 1, "measured_comparison_available": False,
        "failed_case": result, "ling_revision_operator_asserted": environment["ling_revision_operator_asserted"],
        "tool_checkout_head": environment["tool_checkout_head"], "tool_checkout_status": environment["tool_checkout_status"],
        "inputs": environment["inputs"], "redis_pin": environment["redis_pin"],
        "diagnosis": "Recorded follow-up test-tool diagnosis: SET PX TTL used the generic UInt encoding instead of the required signed Int. Commit 5c60eec corrected the benchmark encoder; original failure evidence is preserved here.",
        "limitations": ["Only the first ling Reliable/string/SET PX warmup case ran; no Redis case and no valid comparison pair completed.",
                        "Two ERR_INVALID_ARGUMENT responses, worker cancellations and BrokenBarrierError are retained; no measured throughput or latency is claimed.",
                        "This is a test-tool encoding failure record, not evidence of a product performance regression."]}
    smoke_summary = {"run_id": LAYOUT.name, "controller_exit": int(LAYOUT.with_name(LAYOUT.name + "-exit.txt").read_text()),
        "orchestration_passed": layout["orchestration_passed"], "profile": profile, "provenance": provenance,
        "driver_exits": layout["driver_exits"], "remaining_processes": layout["remaining_processes"],
        "cleanup_events": layout["cleanup_events"], "cleanup_signals_observed": sum(row.get("action") == "signal" for row in layout["cleanup_events"]),
        "common_actual_shadow_interval": common, "all_ten_pressure_intervals_covered": True, "phases": phases,
        "limitations": ["This is a 90-second shadow / 10-second-per-policy Linux orchestration smoke test, neither 3300-second calibration nor 72-hour endurance acceptance.",
                        "Normal completion emitted no cleanup signals; this run did not exercise forced-stop or failure cleanup paths.",
                        "The smoke run does not establish long-run resource trends, slow-consumer reconnect cycles, restart recovery, physical power-loss safety, or production readiness.",
                        "The tested binary is the released 0.2.0 artifact, not a released 0.3.0 artifact."]}
    save(OUT / "summary.json", {"redis_first_failure": failure_summary, "layout_smoke": smoke_summary,
        "archive_scope": "Original preflight failure and Linux orchestration smoke; no new workload was executed while archiving. Windows copies do not establish original Linux mode/owner metadata."})
    files, verification = [], []
    for name, source in (("redis-first-failure", REDIS), ("layout-smoke", LAYOUT)):
        rows, checked = archive(name, source)
        files.extend(rows)
        verification.append(checked)
        print(json.dumps(checked), flush=True)
    save(OUT / "evidence-files.manifest.json", {"files": files})
    save(OUT / "archive-verification.json", {"method": "temporary extraction, exact SHA256/size, full archive and nested gzip EOF, source rehash", "archives": verification})
    manifest = [{"path": path.name, "bytes": path.stat().st_size, "sha256": sha256(path)} for path in sorted(OUT.iterdir()) if path.is_file()]
    save(OUT / "manifest.json", {"archived_utc": datetime.now(timezone.utc).isoformat(), "files": manifest})
    for row in manifest:
        path = OUT / row["path"]
        if path.stat().st_size != row["bytes"] or sha256(path) != row["sha256"]:
            raise RuntimeError("Final file manifest mismatch")
    # 验证 Git 路径过滤不改变证据字节；Check Git path filters preserve the bytes before any staging.
    for path in sorted(OUT.iterdir()):
        raw = subprocess.check_output(["git", "hash-object", "--no-filters", str(path)], cwd=ROOT, text=True).strip()
        filtered = subprocess.check_output(["git", "hash-object", "--path=" + path.relative_to(ROOT).as_posix(), str(path)], cwd=ROOT, text=True).strip()
        if raw != filtered:
            raise RuntimeError(f"Git filters alter evidence: {path}")
    print(json.dumps({"destination": str(OUT), "raw_files": len(files), "manifest_files": len(manifest),
                      "git_path_filter_bytes_unchanged": True}), flush=True)


if __name__ == "__main__":
    main()
