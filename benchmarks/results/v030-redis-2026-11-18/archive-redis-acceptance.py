#!/usr/bin/env python3
"""归档一次完整 Redis 对照，不执行受测代码；Archive one complete Redis comparison without running evidence code."""
import argparse
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
CHUNK_BYTES = 64 * 1024 * 1024
ARCHIVE_LIMIT = 72 * 1024 * 1024
IDENTITY = ("mode", "kind", "operation", "repeat", "system")


def sha256(path):
    """流式核对原件；Hash original bytes with bounded memory."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def load(path):
    """只加载普通 JSON，不执行输入或反序列化对象；Load plain JSON, never execute or deserialize objects."""
    return json.loads(path.read_text(encoding="utf-8-sig"))


def save(path, value):
    """仅写新归档目录；Write only the fresh archive destination."""
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def files_under(directory):
    """拒绝链接和非普通文件；Reject links and non-regular evidence files."""
    if directory.is_symlink() or not directory.is_dir():
        raise RuntimeError(f"Missing or linked evidence directory: {directory}")
    result = []
    for path in sorted(directory.rglob("*")):
        if path.is_symlink() or not (path.is_dir() or path.is_file()):
            raise RuntimeError(f"Non-regular evidence path: {path}")
        if path.is_file():
            result.append(path)
    return result


def record(path, name):
    """完整清单保留原路径与修改时间；Keep source location and modification time in the exact inventory."""
    if path.is_symlink() or not path.is_file():
        raise RuntimeError(f"Missing or linked evidence file: {path}")
    relative = PurePosixPath(name)
    if relative.is_absolute() or ".." in relative.parts or "\\" in name:
        raise RuntimeError(f"Unsafe archive name: {name}")
    stat = path.stat()
    return {"path": name, "source_path": str(path), "bytes": stat.st_size,
            "sha256": sha256(path), "source_mtime_ns": stat.st_mtime_ns}


def check_unchanged(row):
    """完成后再读全部原件；Rehash every original after archiving."""
    path = Path(row["source_path"])
    if (path.is_symlink() or path.stat().st_size != row["bytes"]
            or path.stat().st_mtime_ns != row["source_mtime_ns"] or sha256(path) != row["sha256"]):
        raise RuntimeError(f"Source changed during archival: {path}")


def exit_code(path):
    """兼容已有 UTF-8 和 PowerShell UTF-16 退出文件；Read UTF-8 or PowerShell UTF-16 exit records."""
    raw = path.read_bytes()
    return int(raw.decode("utf-16" if raw.startswith((b"\xff\xfe", b"\xfe\xff")) else "utf-8-sig").strip())


def verify_archive(path, rows):
    """实际解压每件并读完内外 gzip；Extract every member and read inner/outer gzip through CRC/EOF."""
    expected, seen, nested = {row["path"]: row for row in rows}, set(), []
    with tempfile.TemporaryDirectory(prefix="ling-redis-archive-") as temporary:
        with tarfile.open(path, "r:gz") as bundle:
            for member in bundle:
                name = PurePosixPath(member.name)
                if (not member.isfile() or name.is_absolute() or ".." in name.parts
                        or "\\" in member.name or member.name not in expected or member.name in seen):
                    raise RuntimeError(f"Unexpected or unsafe archive member: {member.name}")
                row = expected[member.name]
                if member.size != row["bytes"]:
                    raise RuntimeError(f"Archive member size mismatch: {member.name}")
                extracted = Path(temporary).joinpath(*name.parts)
                extracted.parent.mkdir(parents=True, exist_ok=True)
                with bundle.extractfile(member) as source, extracted.open("xb") as target:
                    shutil.copyfileobj(source, target, 1024 * 1024)
                if extracted.stat().st_size != row["bytes"] or sha256(extracted) != row["sha256"]:
                    raise RuntimeError(f"Extracted SHA/size mismatch: {member.name}")
                if extracted.suffix == ".gz":
                    with gzip.open(extracted, "rb") as source:
                        while source.read(1024 * 1024):
                            pass
                    nested.append(member.name)
                seen.add(member.name)
    if seen != set(expected):
        raise RuntimeError(f"Archive member set mismatch: {path.name}")
    with gzip.open(path, "rb") as source:
        while source.read(1024 * 1024):
            pass
    return {"archive": path.name, "members": len(seen), "source_bytes": sum(row["bytes"] for row in rows),
            "bytes": path.stat().st_size, "sha256": sha256(path), "extracted_sha256_size_match": True,
            "gzip_crc_and_eof_verified": True, "nested_gzip_crc_and_eof_verified": nested}


def arguments():
    """默认当前一次验收，路径可显式替换；Default to the current one-run acceptance with explicit path overrides."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=ROOT / "target/redis-full-20261118-0507")
    parser.add_argument("--output", type=Path, default=ROOT / "benchmarks/results/v030-redis-2026-11-18")
    parser.add_argument("--redis-provenance", type=Path, default=ROOT / "target/redis-8.10.3-build-evidence")
    parser.add_argument("--audit", type=Path)
    parser.add_argument("--audit-execution", type=Path)
    parser.add_argument("--audit-stderr", type=Path)
    parser.add_argument("--controller-log", type=Path)
    parser.add_argument("--exit-file", type=Path)
    parser.add_argument("--analyzer", type=Path, default=ROOT / "target/analyze-redis-results.py")
    parser.add_argument("--analyzer-tests", type=Path, default=ROOT / "target/test_analyze_redis_results.py")
    parser.add_argument("--test-report", type=Path, default=ROOT / "target/redis-analyzer-tests.json")
    parser.add_argument("--test-stdout", type=Path, default=ROOT / "target/redis-analyzer-tests.stdout.bin")
    parser.add_argument("--test-stderr", type=Path, default=ROOT / "target/redis-analyzer-tests.stderr.bin")
    parser.add_argument("--benchmark-tests-log", type=Path, default=ROOT / "target/redis-corrected-wsl-tests.log")
    args = parser.parse_args()
    args.source, args.output = args.source.resolve(), args.output.resolve()
    for field, suffix in (("audit", "audit.json"), ("audit_execution", "audit-execution.json"),
                          ("audit_stderr", "audit.stderr.bin"), ("controller_log", "controller.log"), ("exit_file", "exit.txt")):
        if getattr(args, field) is None:
            setattr(args, field, args.source.with_name(args.source.name + "-" + suffix))
    return args


def main():
    """归档仅消费已有结果与测试记录，不调用 WSL、测试、构建或分析器；Consume existing evidence without running WSL/tests/builds/analyzer."""
    args = arguments()
    source, output = args.source, args.output
    if output.exists() or not output.is_relative_to(ROOT) or output.is_relative_to(source):
        raise RuntimeError("Destination must be a new repository directory outside the source tree")
    source_files = files_under(source)
    environment, original = load(source / "environment.json"), load(source / "summary.json")
    results, schedule = load(source / "results.json"), load(source / "schedule.json")
    audit, audit_execution, tests = load(args.audit), load(args.audit_execution), load(args.test_report)
    expected = {(mode, kind, operation, repeat, system) for mode in ("fast", "reliable")
                for kind in ("string", "bytes") for operation in ("get", "set", "set-px")
                for repeat in (1, 2, 3) for system in ("ling", "redis")}
    case_names = {"-".join(map(str, identity)) for identity in expected}
    for label, rows in (("results", results), ("schedule", schedule)):
        identities = [tuple(row[field] for field in IDENTITY) for row in rows]
        if len(identities) != 72 or len(set(identities)) != 72 or set(identities) != expected:
            raise RuntimeError(f"Cannot archive as a complete matrix: {label} is not the exact 72 cases")
    if {path.name for path in source.iterdir() if path.is_dir()} != case_names | {"inputs"}:
        raise RuntimeError("Missing or unexpected case directories")
    if type(audit.get("valid")) is not bool or audit.get("case_count") != 72 or audit.get("group_count") != 24:
        raise RuntimeError("Independent analysis must actually report 72 cases and 24 groups")
    if (len(audit.get("cases", [])) != 72 or {row["name"] for row in audit["cases"]} != case_names
            or len(audit.get("groups", [])) != 24):
        raise RuntimeError("Independent analysis case/group inventory is incomplete")
    analyzer_digest, test_digest = sha256(args.analyzer), sha256(args.analyzer_tests)
    if (audit_execution.get("analyzer_sha256") != analyzer_digest
            or audit_execution.get("audit_sha256") != sha256(args.audit)
            or audit_execution.get("exit_code") != (0 if audit["valid"] else 1)):
        raise RuntimeError("Independent analysis execution receipt does not bind this analyzer/report/outcome")
    if (tests.get("analyzer_sha256") != analyzer_digest or tests.get("tests_sha256") != test_digest
            or tests.get("exit_code") != 0):
        raise RuntimeError("Offline analyzer test receipt is missing, stale or unsuccessful")
    # 官方源下载记录与构建记录均保留，但不将其升级为独立重建证明。
    # Keep official download/build records without claiming an independently reproduced build.
    provenance_files = files_under(args.redis_provenance)
    required_provenance = {"download-manifest.json", "official-hashes-README.txt", "official-release.json",
                           "official-tag.json", "redis-8.10.3.tar.gz", "build-metadata.json", "build.log"}
    if not required_provenance <= {path.relative_to(args.redis_provenance).as_posix() for path in provenance_files}:
        raise RuntimeError("Redis official source/build provenance is incomplete")
    build, download = load(args.redis_provenance / "build-metadata.json"), load(args.redis_provenance / "download-manifest.json")
    inputs = environment["inputs"]
    if (build.get("exit_code") != 0 or build.get("binary_sha256") != inputs["redis"]["sha256"]
            or build.get("source_archive_sha256") != inputs["redis_source_archive"]["sha256"]):
        raise RuntimeError("Redis build receipt does not match the frozen measured input")
    for entry in download["files"].values():
        path = args.redis_provenance / entry["file"]
        if path.stat().st_size != entry["bytes"] or sha256(path) != entry["sha256"]:
            raise RuntimeError(f"Official download inventory mismatch: {path.name}")
    for name in case_names:
        for filename in ("result.json", "server.log", "latencies-ns.json.gz"):
            if not (source / name / filename).is_file():
                raise RuntimeError(f"Required case evidence absent: {name}/{filename}")
    packed = [record(path, "raw/" + path.relative_to(source).as_posix()) for path in source_files]
    by_name = {row["path"]: row for row in packed}
    for label, value in inputs.items():
        row = by_name.get("raw/" + value["copy"])
        if row is None or row["sha256"] != value["sha256"] or audit["inputs"][label]["sha256"] != row["sha256"]:
            raise RuntimeError(f"Frozen input or independent audit input mismatch: {label}")
    for case in audit["cases"]:
        row = by_name["raw/" + case["name"] + "/latencies-ns.json.gz"]
        if row["sha256"] != case.get("raw_sha256"):
            raise RuntimeError(f"Independent latency audit identity mismatch: {case['name']}")
    # 原始运行的输入保留一次；外部重复源包/二进制用可验证的别名保留来源。
    # Preserve frozen source/binary inputs once; retain duplicate external locations as verified aliases.
    dedup = {(by_name["raw/" + inputs[label]["copy"]]["bytes"], inputs[label]["sha256"]):
             "raw/" + inputs[label]["copy"] for label in ("ling", "redis", "redis_source_archive")}
    aliases = []
    for path in provenance_files:
        row = record(path, "redis-provenance/" + path.relative_to(args.redis_provenance).as_posix())
        target = dedup.get((row["bytes"], row["sha256"]))
        if target:
            row["stored_as"] = target
            aliases.append(row)
        else:
            packed.append(row)
    packed += [record(path, "execution/" + name) for path, name in (
        (args.controller_log, "controller.log"), (args.exit_file, "exit.txt"),
        (args.audit_execution, "audit-execution.json"), (args.audit_stderr, "audit.stderr.bin"),
        (args.test_report, "analyzer-tests.json"), (args.test_stdout, "analyzer-tests.stdout.bin"),
        (args.test_stderr, "analyzer-tests.stderr.bin"), (args.benchmark_tests_log, "benchmark-tests.log"))]
    direct = [record(path, name) for path, name in ((args.audit, "audit.json"),
              (args.analyzer, "analyze-redis-results.py"), (args.analyzer_tests, "test_analyze_redis_results.py"),
              (Path(__file__), "archive-redis-acceptance.py"))]
    if len({row["path"] for row in packed + aliases + direct}) != len(packed + aliases + direct):
        raise RuntimeError("Evidence names collide")
    if any(row["bytes"] + 2048 > CHUNK_BYTES for row in packed):
        raise RuntimeError("A raw file exceeds the 64 MiB per-chunk budget; do not split or omit it silently")
    controller_exit = exit_code(args.exit_file)
    # 只有真实独立分析和运行结果均有效才能报告完整对照有效。
    # Claim a valid complete comparison only when actual independent analysis and run results agree.
    valid = (audit["valid"] is True and not audit["errors"] and controller_exit == 0
             and original.get("valid") is True and original.get("planned_cases") == 72
             and original.get("executed_cases") == 72)
    output.mkdir(parents=True)
    (output / ".gitattributes").write_bytes(b"* -text whitespace=cr-at-eol\n*.gz -diff\n*.bin -diff\n")
    for row in direct:
        shutil.copyfile(row["source_path"], output / row["path"])
        row["storage"] = "direct"
    chunks, current, current_bytes = [], [], 0
    for row in packed:
        cost = ((row["bytes"] + 511) // 512) * 512 + 2048
        if current and current_bytes + cost > CHUNK_BYTES:
            chunks.append(current)
            current, current_bytes = [], 0
        current.append(row)
        current_bytes += cost
    if current:
        chunks.append(current)
    verified = []
    for number, chunk in enumerate(chunks, 1):
        archive = output / f"raw-{number:03}.tar.gz"
        with tarfile.open(archive, "w:gz", compresslevel=6) as bundle:
            for row in chunk:
                row["archive"] = archive.name
                row["storage"] = "archive"
                bundle.add(row["source_path"], arcname=row["path"], recursive=False)
        if archive.stat().st_size >= ARCHIVE_LIMIT:
            raise RuntimeError(f"Archive exceeds the 72 MiB hard ceiling: {archive.name}")
        verified.append(verify_archive(archive, chunk))
        print(json.dumps(verified[-1]), flush=True)
    stored = {row["path"]: row for row in packed}
    for row in aliases:
        row["storage"] = "alias"
        row["archive"] = stored[row["stored_as"]]["archive"]
    for row in packed + aliases + direct:
        check_unchanged(row)
    if files_under(source) != source_files or files_under(args.redis_provenance) != provenance_files:
        raise RuntimeError("Source file set changed during archival")
    for row in direct:
        copied = output / row["path"]
        if copied.stat().st_size != row["bytes"] or sha256(copied) != row["sha256"]:
            raise RuntimeError(f"Direct evidence copy mismatch: {row['path']}")
    save(output / "evidence-files.manifest.json", {"source_root": str(source), "files": packed + direct,
         "deduplicated_source_aliases": aliases, "source_file_count": len(source_files),
         "scope": "Every original run file is individually preserved; only duplicate external frozen source/binary inputs use aliases."})
    save(output / "summary.json", {"run_id": source.name, "valid": valid, "controller_exit": controller_exit,
         "independent_audit_valid": audit["valid"], "case_count": audit["case_count"], "group_count": audit["group_count"],
         "independent_audit_errors": audit["errors"], "groups": audit["groups"],
         "ling_revision_operator_asserted": environment.get("ling_revision_operator_asserted"),
         "input_identity": audit["inputs"], "identity_checks": audit.get("identity_checks"),
         "tool_checkout_head": environment.get("tool_checkout_head"), "tool_checkout_status": environment.get("tool_checkout_status"),
         "redis_pin": environment.get("redis_pin"), "redis_build_metadata": build,
         "comparability": original.get("comparability"), "audit_scope": audit.get("scope"),
         "limitations": ["Pre-release closed-loop same-host comparison of ling 0.2.0 and the pinned Redis build; no 0.3.0 release or post-release acceptance is implied.",
             "Fast persistence and Reliable durability objects differ; report each system/scenario separately, never a single overall ranking.",
             "CPU values, RSS endpoints and lifetime VmHWM retain the original measurement boundaries; no continuous peaks, saturation, leak absence or endurance claim.",
             "Official source and successful build records bind recorded inputs; this archiver does not independently reproduce the build.",
             "Windows copies preserve file bytes, not original Linux owner/mode metadata. No WSL, tests, builds, analyzer or load is started by this script."]})
    save(output / "archive-verification.json", {"verified_utc": datetime.now(timezone.utc).isoformat(),
         "archives": verified, "all_originals_rehashed_after_archiving": True,
         "direct_copies_sha256_size_match": True, "nested_gzip_count": sum(len(row["nested_gzip_crc_and_eof_verified"]) for row in verified),
         "method": "Complete exact SHA256/size inventory; safe temporary extraction of every member; full outer and nested gzip CRC/EOF; source rehash; direct-copy rehash."})
    # 本地 attributes 必须在 git 过滤校验前存在；Local attributes precede the Git byte-preservation check.
    for path in sorted(output.iterdir()):
        raw = subprocess.check_output(["git", "hash-object", "--no-filters", str(path)], cwd=ROOT, text=True).strip()
        filtered = subprocess.check_output(["git", "hash-object", "--path=" + path.relative_to(ROOT).as_posix(), str(path)], cwd=ROOT, text=True).strip()
        if raw != filtered:
            raise RuntimeError(f"Git path filters alter evidence bytes: {path.name}")
    manifest = {"archived_utc": datetime.now(timezone.utc).isoformat(), "run_id": source.name,
         "archive_count": len(verified), "valid_comparison": valid, "source_file_count": len(source_files),
         "git_path_filter_bytes_unchanged": True,
         "files": [{"path": path.name, "bytes": path.stat().st_size, "sha256": sha256(path)}
                   for path in sorted(output.iterdir()) if path.is_file()]}
    save(output / "manifest.json", manifest)
    for row in manifest["files"]:
        path = output / row["path"]
        if path.stat().st_size != row["bytes"] or sha256(path) != row["sha256"]:
            raise RuntimeError(f"Final outer manifest mismatch: {path.name}")
    manifest_path = output / "manifest.json"
    if subprocess.check_output(["git", "hash-object", "--no-filters", str(manifest_path)], cwd=ROOT) != subprocess.check_output(
            ["git", "hash-object", "--path=" + manifest_path.relative_to(ROOT).as_posix(), str(manifest_path)], cwd=ROOT):
        raise RuntimeError("Git path filters alter the final manifest")
    print(json.dumps({"output": str(output), "archives": len(verified), "valid_comparison": valid,
                      "cases": audit["case_count"], "groups": audit["group_count"],
                      "manifest_sha256": sha256(manifest_path), "no_workload_started": True}), flush=True)


if __name__ == "__main__":
    main()
