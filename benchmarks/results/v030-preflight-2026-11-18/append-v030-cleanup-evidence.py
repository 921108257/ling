"""追加真实定向清理烟测并保留旧归档；Append cleanup-smoke evidence without rewriting existing archives."""
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
SOURCE = ROOT / "target/layout-cleanup-smoke-20261118-0504"
ARCHIVE = OUT / "cleanup-smoke.tar.gz"


def sha256(path):
    """流式核验完整原始字节；Verify complete raw bytes using bounded memory."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def load(path):
    """结构化证据为 UTF-8；Structured evidence uses UTF-8."""
    return json.loads(path.read_text(encoding="utf-8"))


def save(path, value):
    """只替换本次允许更新的摘要与清单；Replace only the permitted summary and manifests."""
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def verify_archive(path, files):
    """逐件实际解压核验并完整读取外层及内层 gzip；Extract, hash, and read every gzip through EOF."""
    expected = {row["path"]: row for row in files}
    seen, nested = set(), 0
    with tempfile.TemporaryDirectory(prefix="ling-cleanup-verify-") as temporary:
        with tarfile.open(path, "r:gz") as bundle:
            for member in bundle:
                relative = PurePosixPath(member.name)
                if (not member.isfile() or relative.is_absolute() or ".." in relative.parts
                        or member.name in seen or member.name not in expected):
                    raise RuntimeError(f"Unexpected or unsafe member: {member.name}")
                target = Path(temporary).joinpath(*relative.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                with bundle.extractfile(member) as source, target.open("xb") as destination:
                    shutil.copyfileobj(source, destination, 1024 * 1024)
                row = expected[member.name]
                if target.stat().st_size != row["bytes"] or sha256(target) != row["sha256"]:
                    raise RuntimeError(f"Archive member mismatch: {member.name}")
                if target.suffix == ".gz":
                    with gzip.open(target, "rb") as stream:
                        while stream.read(1024 * 1024):
                            pass
                    nested += 1
                seen.add(member.name)
    if seen != set(expected):
        raise RuntimeError("Archive member set differs")
    with gzip.open(path, "rb") as source:
        while source.read(1024 * 1024):
            pass
    return {"archive": path.name, "members": len(seen), "source_bytes": sum(row["bytes"] for row in files),
            "extracted_sha256_size_match": True, "gzip_read_to_eof": True, "nested_gzip_read_to_eof": nested}


def main():
    """仅追加，不启动 WSL、服务或测试；Only append evidence, never start WSL, servers or tests."""
    manifest = load(OUT / "manifest.json")
    for row in manifest["files"]:
        path = OUT / row["path"]
        if path.stat().st_size != row["bytes"] or sha256(path) != row["sha256"]:
            raise RuntimeError(f"Existing manifest mismatch: {path.name}")
    if ARCHIVE.exists():
        raise RuntimeError("Refuse to overwrite the cleanup archive")
    previous_archives = {name: sha256(OUT / name) for name in ("redis-first-failure.tar.gz", "layout-smoke.tar.gz")}
    check, result = load(SOURCE / "cleanup-smoke-result.json"), load(SOURCE / "layout-result.json")
    if check["passed"] is not True or check["orchestrator_exit"] != 1 or result["orchestration_passed"] is not False:
        raise RuntimeError("Expected controller failure was not recorded")
    if not any("intentional cleanup smoke after worker progress" in error for error in result["errors"]):
        raise RuntimeError("Expected failure injection absent")
    if len(result["driver_exits"]) != 2 or any(row["exit_code"] != 130 for row in result["driver_exits"]) or result["remaining_processes"]:
        raise RuntimeError("Unexpected driver exits or remaining processes")
    signals = result["cleanup_events"]
    owners = {row["driver"]["pid"]: row["driver"] for row in result["owned_process_identities"]}
    if len(signals) != 2 or any(row.get("action") != "signal" or row.get("signal") != 2 for row in signals):
        raise RuntimeError("Expected two SIGINT events")
    if any({key: row[key] for key in ("pid", "pgrp", "session", "start_ticks")} != owners.get(row["pid"]) for row in signals):
        raise RuntimeError("Signal target does not match recorded owned driver identity")
    phases = []
    for mode in ("fast", "reliable"):
        directory = SOURCE / (mode + "-shadow") / (mode + "-shadow-noeviction")
        row, progress = load(directory / "result.json"), load(directory / "progress.json")
        if row["status"] != "interrupted" or row["server_exit_code"] != 0:
            raise RuntimeError("Expected interrupted phase with clean server exit")
        cycles = [progress["counts"].get(f"worker_{worker}_cycles", 0) for worker in range(2)]
        if any(value <= 0 for value in cycles):
            raise RuntimeError("Worker progress was not recorded before cleanup")
        phases.append({"name": mode + "-shadow-noeviction", "status": row["status"],
                       "server_exit_code": row["server_exit_code"], "actual_load_seconds": row["load_stop_requested_elapsed_seconds"],
                       "worker_cycles_at_last_progress": cycles, "findings": row["findings"], "criteria": row["criteria"]})
    frozen = load(SOURCE / "layout.json")
    for source_path, digest in frozen["script_and_tool_sha256"].items():
        if sha256(SOURCE / "tool_source" / Path(source_path).name) != digest:
            raise RuntimeError("Frozen cleanup tool mismatch")
    sources = [(path, "raw/" + path.relative_to(SOURCE).as_posix()) for path in sorted(SOURCE.rglob("*")) if path.is_file()]
    if any(path.is_symlink() for path in SOURCE.rglob("*")):
        raise RuntimeError("Symlinks are not allowed")
    execution_names = ["smoke-v030-cleanup.py", "layout-cleanup-smoke-20261118-0504-controller.log",
                       "layout-cleanup-smoke-20261118-0504-exit.txt", "redis-corrected-wsl-tests.log", "layout-wsl-tests.log"]
    sources += [(ROOT / "target" / name, "execution/" + name) for name in execution_names]
    if sha256(ROOT / "target/smoke-v030-cleanup.py") != sha256(SOURCE / "smoke-v030-cleanup.py"):
        raise RuntimeError("External and frozen cleanup harness differ")
    historical_tests = []
    for name, count in (("redis-corrected-wsl-tests.log", 20), ("layout-wsl-tests.log", 27)):
        path = ROOT / "target" / name
        if f"Ran {count} tests".encode() not in path.read_bytes() or b"\nOK" not in path.read_bytes():
            raise RuntimeError(f"Expected historical test report missing: {name}")
        historical_tests.append({"archive_path": "execution/" + name, "reported_test_count": count,
                                 "reported_result": "OK", "sha256": sha256(path), "executed_by_this_archiver": False,
                                 "scope": "pre-existing WSL offline test log; raw mixed-encoding wrapper output retained"})
    files = [{"archive": ARCHIVE.name, "path": name, "source_path": str(path.relative_to(ROOT)),
              "bytes": path.stat().st_size, "sha256": sha256(path), "source_mtime_ns": path.stat().st_mtime_ns}
             for path, name in sources]
    with tarfile.open(ARCHIVE, "w:gz", compresslevel=6) as bundle:
        for path, name in sources:
            bundle.add(path, arcname=name, recursive=False)
    if ARCHIVE.stat().st_size >= 90 * 1024 * 1024:
        raise RuntimeError("Archive exceeds 90 MiB")
    verified = verify_archive(ARCHIVE, files)
    for (path, _), row in zip(sources, files):
        if path.stat().st_size != row["bytes"] or sha256(path) != row["sha256"]:
            raise RuntimeError(f"Source changed during archival: {path}")
    verified["source_unchanged"] = True
    evidence = load(OUT / "evidence-files.manifest.json")
    evidence["files"].extend(files)
    save(OUT / "evidence-files.manifest.json", evidence)
    verification = load(OUT / "archive-verification.json")
    verification["archives"].append(verified)
    # 所有三档本轮重新实际解压；Re-extract all three archives during this append operation.
    verification["append_all_archive_verification"] = [verify_archive(OUT / name, [row for row in evidence["files"] if row["archive"] == name])
                                                       for name in (*previous_archives, ARCHIVE.name)]
    save(OUT / "archive-verification.json", verification)
    summary = load(OUT / "summary.json")
    summary["cleanup_smoke"] = {"run_id": SOURCE.name, "check": check,
        "controller_exit": int((ROOT / "target/layout-cleanup-smoke-20261118-0504-exit.txt").read_text()),
        "orchestration_passed": result["orchestration_passed"], "orchestrator_exit": check["orchestrator_exit"],
        "errors": result["errors"], "coverage": result["coverage"], "cleanup_events": signals,
        "driver_exits": result["driver_exits"], "owned_process_identities": result["owned_process_identities"],
        "remaining_processes": result["remaining_processes"], "phases": phases, "historical_offline_test_logs": historical_tests,
        "scope": "Intentional controller failure after two owned shadows started. Two verified pidfd SIGINT signals; both drivers exit 130 and both servers exit 0, with no remaining processes.",
        "limitations": ["passed=true means only the intentional-failure cleanup check passed. The original orchestration and coverage results remain false.",
                        "Neither pressure driver started; this is not workload completion, calibration or endurance acceptance.",
                        "This run exercised cooperative SIGINT cleanup, not the SIGKILL escalation path or physical power failure.",
                        "The WSL offline test logs predate archiving. No tests, WSL, services or load were started by this append operation."]}
    save(OUT / "summary.json", summary)
    shutil.copyfile(__file__, OUT / "append-v030-cleanup-evidence.py")
    for name, digest in previous_archives.items():
        if sha256(OUT / name) != digest:
            raise RuntimeError("An existing archive changed")
    manifest["updated_utc"] = datetime.now(timezone.utc).isoformat()
    manifest["files"] = [{"path": path.name, "bytes": path.stat().st_size, "sha256": sha256(path)}
                         for path in sorted(OUT.iterdir()) if path.is_file() and path.name != "manifest.json"]
    save(OUT / "manifest.json", manifest)
    for row in manifest["files"]:
        path = OUT / row["path"]
        if path.stat().st_size != row["bytes"] or sha256(path) != row["sha256"]:
            raise RuntimeError("Updated outer manifest differs")
    for path in OUT.iterdir():
        raw = subprocess.check_output(["git", "hash-object", "--no-filters", str(path)], cwd=ROOT, text=True).strip()
        filtered = subprocess.check_output(["git", "hash-object", "--path=" + path.relative_to(ROOT).as_posix(), str(path)], cwd=ROOT, text=True).strip()
        if raw != filtered:
            raise RuntimeError("Git filter changes evidence bytes")
    print(json.dumps({"archive": ARCHIVE.name, "bytes": ARCHIVE.stat().st_size, "sha256": sha256(ARCHIVE),
                      "members": len(files), "previous_archives_unchanged": True,
                      "all_three_archives_extracted_and_gzip_verified": True,
                      "manifest_sha256": sha256(OUT / "manifest.json"), "summary_sha256": sha256(OUT / "summary.json"),
                      "no_wsl_tests_or_load_started": True}), flush=True)


if __name__ == "__main__":
    main()
