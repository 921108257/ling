#!/usr/bin/env python3
"""只归档已停止的校准与既有 RSS 预检；Archive stopped calibration and existing RSS preflight evidence only."""
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
import zlib


ROOT = Path(__file__).resolve().parents[1]
CHUNK_LIMIT = 64 * 1024 * 1024
COMPRESSED_LIMIT = 72 * 1024 * 1024
MODES = ("fast", "reliable")
POLICIES = ("noeviction", "lru", "lfu", "random", "ttl-first")
EXTRA_RESULT_FIELDS = ("event_mode", "phase", "policy", "seed", "status", "requested_seconds",
    "load_started_monotonic_seconds", "load_stop_requested_elapsed_seconds", "elapsed_seconds",
    "server_exit_code", "shutdown_method", "criteria", "counts", "findings", "server_errors",
    "consumers", "consumer_acceptance", "workload_degraded_by_resource_limit", "server_log_capture_complete",
    "server_panic_markers")


def sha256(path):
    """有界内存读取完整原件；Read complete originals with bounded memory."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def load(path, diagnostics):
    """保留元数据错误，不执行证据；Keep metadata errors without executing evidence."""
    try:
        if path.stat().st_size > 64 * 1024 * 1024:
            raise ValueError("metadata exceeds 64 MiB")
        result = json.loads(path.read_text(encoding="utf-8-sig"))
        if not isinstance(result, dict):
            raise ValueError("metadata is not a JSON object")
        return result
    except (OSError, ValueError) as error:
        diagnostics.append({"path": str(path), "error": str(error)})
        return {}


def save(path, value):
    """只写全新输出目录；Write only into the new output directory."""
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def files_under(directory):
    """输入只允许普通文件与目录；Accept only regular files and directories."""
    if directory.is_symlink() or not directory.is_dir():
        raise RuntimeError(f"Missing or linked source directory: {directory}")
    result = []
    for path in sorted(directory.rglob("*")):
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise RuntimeError(f"Non-regular source: {path}")
        if path.is_file():
            result.append(path)
    return result


def record(path, name):
    """逐原路径记录字节、摘要与时间；Inventory each original path, size, digest and time."""
    name_parts = PurePosixPath(name)
    if path.is_symlink() or not path.is_file() or name_parts.is_absolute() or ".." in name_parts.parts or "\\" in name:
        raise RuntimeError(f"Missing or unsafe evidence: {path} -> {name}")
    stat = path.stat()
    return {"path": name, "source_path": str(path), "bytes": stat.st_size,
            "sha256": sha256(path), "source_mtime_ns": stat.st_mtime_ns}


def recheck(row):
    """归档后再次读取源文件；Read every original again after archiving."""
    path = Path(row["source_path"])
    if (path.is_symlink() or path.stat().st_size != row["bytes"]
            or path.stat().st_mtime_ns != row["source_mtime_ns"] or sha256(path) != row["sha256"]):
        raise RuntimeError(f"Source changed during archival: {path}")


def read_exit(path, diagnostics):
    """保留 UTF-8/UTF-16 退出记录及未知值；Keep UTF-8/UTF-16 exit evidence and unknown outcomes."""
    try:
        raw = path.read_bytes()
        return int(raw.decode("utf-16" if raw.startswith((b"\xff\xfe", b"\xfe\xff")) else "utf-8-sig").strip())
    except (OSError, ValueError) as error:
        diagnostics.append({"path": str(path), "error": str(error)})
        return None


def validate_extra(extra, source, analysis_path, phase_metadata):
    """将补充报告绑定当前根与十二阶段，失败状态本身不拒绝；Bind current root and twelve phases without rejecting recorded failures."""
    if extra.get("format") != "ling-v030-calibration-supplement-1":
        raise RuntimeError("Unexpected supplementary analysis format")
    for field, expected in (("root", source), ("analysis_report", analysis_path)):
        recorded = extra.get(field)
        if not isinstance(recorded, str) or Path(recorded).resolve() != expected.resolve():
            raise RuntimeError(f"Supplementary analysis {field} differs from current source")
    reported = extra.get("phases")
    if not isinstance(reported, list) or len(reported) != 12 or len(phase_metadata) != 12:
        raise RuntimeError("Supplementary analysis must describe exactly twelve phases")
    names = [phase.get("name") for phase in reported if isinstance(phase, dict)]
    if len(names) != 12 or len(set(names)) != 12 or set(names) != set(phase_metadata):
        raise RuntimeError("Supplementary phase names are missing, duplicated or unexpected")
    for phase in reported:
        name = phase["name"]
        expected = phase_metadata[name]
        relative = phase.get("relative_directory")
        parts = PurePosixPath(relative.replace("\\", "/")) if isinstance(relative, str) else None
        if parts is None or parts.is_absolute() or ".." in parts.parts or parts.as_posix() != expected["relative_directory"]:
            raise RuntimeError(f"Supplementary phase directory differs: {name}")
        if phase.get("settings") != expected["settings"] or not isinstance(phase.get("result"), dict):
            raise RuntimeError(f"Supplementary phase settings/result differ: {name}")
        for field in EXTRA_RESULT_FIELDS:
            if phase["result"].get(field) != expected["result"].get(field):
                raise RuntimeError(f"Supplementary phase metadata differs: {name}/{field}")
    return {"root_matches": True, "analysis_report_matches": True, "phase_metadata_matches": 12}


def nested_gzip_check(path, name):
    """损坏原件仍可归档，只记录内层解码失败；Archive corrupt originals while recording inner decompression failures."""
    uncompressed = 0
    result = {"path": name, "crc_eof_verified": False}
    try:
        with gzip.open(path, "rb") as source:
            while block := source.read(1024 * 1024):
                uncompressed += len(block)
        result["crc_eof_verified"] = True
    except (OSError, EOFError, zlib.error) as error:
        result.update(error_class=type(error).__name__, error=str(error))
    result["uncompressed_bytes_read"] = uncompressed
    return result


def verify_archive(path, rows):
    """外层及成员字节严格校验，内层损坏单独报告；Verify outer/member integrity strictly, reporting inner damage separately."""
    expected, seen, nested = {row["path"]: row for row in rows}, set(), []
    with tempfile.TemporaryDirectory(prefix="ling-calibration-archive-") as temporary:
        with tarfile.open(path, "r:gz") as bundle:
            for member in bundle:
                name = PurePosixPath(member.name)
                if (not member.isfile() or name.is_absolute() or ".." in name.parts
                        or "\\" in member.name or member.name not in expected or member.name in seen):
                    raise RuntimeError(f"Unexpected or unsafe archive member: {member.name}")
                row = expected[member.name]
                if member.size != row["bytes"]:
                    raise RuntimeError(f"Member size differs: {member.name}")
                target = Path(temporary).joinpath(*name.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                with bundle.extractfile(member) as source, target.open("xb") as destination:
                    shutil.copyfileobj(source, destination, 1024 * 1024)
                if target.stat().st_size != row["bytes"] or sha256(target) != row["sha256"]:
                    raise RuntimeError(f"Extracted bytes differ: {member.name}")
                if target.suffix == ".gz":
                    nested.append(nested_gzip_check(target, member.name))
                seen.add(member.name)
    if seen != set(expected):
        raise RuntimeError(f"Member inventory differs: {path.name}")
    with gzip.open(path, "rb") as source:
        while source.read(1024 * 1024):
            pass
    return {"archive": path.name, "members": len(seen), "original_bytes": sum(row["bytes"] for row in rows),
            "compressed_bytes": path.stat().st_size, "sha256": sha256(path),
            "all_members_extracted_sha256_size_match": True, "outer_gzip_crc_eof": True,
            "nested_gzip_checks": nested,
            "nested_gzip_failure_count": sum(not row["crc_eof_verified"] for row in nested)}


def arguments():
    """当前校准路径可覆盖，绝不自行运行负载；Allow explicit source overrides without starting workloads."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=ROOT / "target/v030-calibration-20261118-0540")
    parser.add_argument("--output", type=Path, default=ROOT / "benchmarks/results/v030-calibration-2026-11-18")
    parser.add_argument("--analysis", type=Path)
    parser.add_argument("--extra", type=Path)
    parser.add_argument("--controller-log", type=Path)
    parser.add_argument("--exit-file", type=Path)
    parser.add_argument("--analyzer", type=Path, default=ROOT / "benchmarks/v030_analyze.py")
    parser.add_argument("--extra-script", type=Path, default=ROOT / "target/analyze-v030-calibration-extra.py")
    parser.add_argument("--historical-prefix", type=Path, default=ROOT / "target/calibration-extra-smoke-20261118-052832")
    parser.add_argument("--historical-analysis", type=Path, default=ROOT / "target/layout-smoke-20261118-0454-analysis.json")
    parser.add_argument("--rss-root", type=Path, default=ROOT / "target/rss-smoke-20261118-0535")
    args = parser.parse_args()
    args.source, args.output = args.source.resolve(), args.output.resolve()
    for field, suffix in (("analysis", "analysis.json"), ("extra", "extra.json"),
                          ("controller_log", "controller.log"), ("exit_file", "exit.txt")):
        if getattr(args, field) is None:
            setattr(args, field, args.source.with_name(args.source.name + "-" + suffix))
    return args


def main():
    """仅在所有驱动停止并获授权后调用；Call only after every driver stops and archiving is authorized."""
    args, diagnostics = arguments(), []
    source, output = args.source, args.output
    if output.exists() or not output.is_relative_to(ROOT) or output.is_relative_to(source) or source.is_relative_to(output):
        raise RuntimeError("Output must be a new repository directory separate from source")
    source_files, rss_files = files_under(source), files_under(args.rss_root)
    analysis, extra = load(args.analysis, diagnostics), load(args.extra, diagnostics)
    layout = load(source / "layout-result.json", diagnostics)
    layout_metadata = load(source / "layout.json", diagnostics)
    controller_exit = read_exit(args.exit_file, diagnostics)
    rows = [record(path, "raw/" + path.relative_to(source).as_posix()) for path in source_files]
    rows += [record(path, "rss-preflight/raw/" + path.relative_to(args.rss_root).as_posix()) for path in rss_files]
    external = [(args.controller_log, "execution/controller.log"), (args.exit_file, "execution/exit.txt"),
                (args.analysis, "analysis.json"), (args.extra, "extra.json"),
                (args.analyzer, "v030_analyze.py"), (args.extra_script, "analyze-v030-calibration-extra.py"),
                (Path(__file__), "archive-v030-calibration.py"),
                (args.historical_analysis, "historical-extra/analysis.json")]
    for suffix in (".json", ".stdout.log", ".stderr.log", ".exit.txt", "-command.json",
                   "-independent.stdout.log", "-independent.stderr.log", "-independent.exit.txt", "-independent-command.json"):
        path = args.historical_prefix.with_name(args.historical_prefix.name + suffix)
        external.append((path, "historical-extra/" + path.name))
    for name in ("v030-rss-sampler-tests.log", "v030-rss-sampler-tests.json", "v030-rss-wsl-tests.log",
                 "v030-rss-wsl-tests-exit.txt", "v030-rss-live-smoke.log", "v030-rss-live-smoke-exit.txt", "smoke-v030-rss.py"):
        external.append((ROOT / "target" / name, "rss-preflight/" + name))
    external += [(ROOT / "benchmarks/v030.py", "rss-preflight/source/v030.py"),
                 (ROOT / "benchmarks/test_v030.py", "rss-preflight/source/test_v030.py")]
    rows += [record(path, name) for path, name in external]
    if len({row["path"] for row in rows}) != len(rows):
        raise RuntimeError("Duplicate archive inventory paths")
    by_path = {row["path"]: row for row in rows}
    # 三个摘要只绑定本次额外分析的输入；上游分析没有逐输入摘要，保留此限制。
    # Three digests bind supplementary analysis inputs; upstream analysis has no per-input hashes.
    bound = {"helper": "analyze-v030-calibration-extra.py", "analyzer": "v030_analyze.py", "analysis_report": "analysis.json"}
    for key, name in bound.items():
        if extra.get("source_sha256", {}).get(key) != by_path[name]["sha256"]:
            raise RuntimeError(f"Supplementary analysis source digest mismatch: {key}")
    phases, binary_paths, phase_metadata = [], set(), {}
    for mode in MODES:
        for kind in ("shadow", "pressure"):
            instance = source / f"{mode}-{kind}"
            environment = load(instance / "environment.json", diagnostics)
            binary = environment.get("binary", {})
            copied = str(binary.get("copied_path", "")).replace("\\", "/").split("/")[-1]
            if copied in ("ling", "ling.exe"):
                name = "raw/" + (instance / copied).relative_to(source).as_posix()
                if name in by_path and by_path[name]["sha256"] == binary.get("sha256"):
                    binary_paths.add(name)
                else:
                    diagnostics.append({"error": "frozen binary missing or digest mismatch", "path": name})
            for policy in (("noeviction",) if kind == "shadow" else POLICIES):
                name = f"{mode}-{kind}-{policy}"
                result_path = instance / name / "result.json"
                result = load(result_path, diagnostics)
                settings = environment.get("settings")
                phase_metadata[name] = {"relative_directory": f"{mode}-{kind}/{name}",
                                        "settings": settings if isinstance(settings, dict) else {}, "result": result}
                phases.append({"name": name, "present": bool(result), "settings": environment.get("settings"),
                    "result": {field: result.get(field) for field in ("event_mode", "phase", "policy", "status",
                        "requested_seconds", "load_started_monotonic_seconds", "load_stop_requested_elapsed_seconds",
                        "elapsed_seconds", "server_exit_code", "shutdown_method", "findings", "server_errors", "criteria",
                        "counts", "consumers", "consumer_acceptance", "workload_degraded_by_resource_limit", "resource_trends",
                        "resource_samples_saved", "native_resources", "server_log_capture_complete", "server_panic_markers")}})
    extra_binding = validate_extra(extra, source, args.analysis, phase_metadata)
    aliases, packed, canonical = [], [], {}
    for row in rows:
        key = (row["bytes"], row["sha256"])
        if row["path"] in binary_paths and key in canonical:
            row["stored_as"] = canonical[key]
            row["storage"] = "exact_binary_alias"
            aliases.append(row)
        else:
            if row["path"] in binary_paths:
                canonical[key] = row["path"]
            packed.append(row)
    # 额外分析与摘要作为直接文件可读，原始数据仍按成员分档。
    # Keep analyses and authoring scripts directly readable; preserve raw evidence as chunk members.
    direct_names = {"analysis.json", "extra.json", "v030_analyze.py", "analyze-v030-calibration-extra.py", "archive-v030-calibration.py"}
    direct = [row for row in packed if row["path"] in direct_names]
    packed = [row for row in packed if row["path"] not in direct_names]
    chunks, current, size = [], [], 10240
    for row in packed:
        cost = ((row["bytes"] + 511) // 512) * 512 + 4096
        if cost + 10240 >= CHUNK_LIMIT:
            raise RuntimeError(f"Single member cannot fit below 64 MiB: {row['path']}")
        if current and size + cost >= CHUNK_LIMIT:
            chunks.append(current)
            current, size = [], 10240
        current.append(row)
        size += cost
    if current:
        chunks.append(current)
    output.mkdir(parents=True)
    (output / ".gitattributes").write_bytes(b"* -text whitespace=cr-at-eol\n*.gz -diff\n*.bin -diff\n")
    for row in direct:
        shutil.copyfile(row["source_path"], output / row["path"])
        row["storage"] = "direct"
    verified = []
    for number, chunk in enumerate(chunks, 1):
        archive = output / f"raw-{number:03}.tar.gz"
        with tarfile.open(archive, "w:gz", compresslevel=6) as bundle:
            for row in chunk:
                row["archive"], row["storage"] = archive.name, "archive"
                bundle.add(row["source_path"], arcname=row["path"], recursive=False)
        with gzip.open(archive, "rb") as stream:
            uncompressed = 0
            while block := stream.read(1024 * 1024):
                uncompressed += len(block)
        if uncompressed >= CHUNK_LIMIT or archive.stat().st_size >= COMPRESSED_LIMIT:
            raise RuntimeError(f"Archive exceeds uncompressed/compressed budget: {archive.name}")
        checked = verify_archive(archive, chunk)
        checked["uncompressed_tar_bytes"] = uncompressed
        verified.append(checked)
        print(json.dumps(checked), flush=True)
    for row in aliases:
        row["archive"] = by_path[row["stored_as"]]["archive"]
    for row in rows:
        recheck(row)
    if files_under(source) != source_files or files_under(args.rss_root) != rss_files:
        raise RuntimeError("Source file inventory changed during archival")
    for row in direct:
        path = output / row["path"]
        if path.stat().st_size != row["bytes"] or sha256(path) != row["sha256"]:
            raise RuntimeError(f"Direct copy mismatch: {row['path']}")
    save(output / "evidence-files.manifest.json", {"source_root": str(source), "source_files": len(source_files),
         "files": packed + direct, "exact_binary_aliases": aliases,
         "scope": "Every original path is inventoried; only verified size+SHA identical frozen binaries use explicit aliases."})
    save(output / "summary.json", {"run_id": source.name, "scope": "calibration evidence only; no 72-hour acceptance asserted",
         "controller_exit": controller_exit, "layout_metadata": layout_metadata, "layout_result": layout,
         "expected_phases": 12, "recorded_phases": sum(phase["present"] for phase in phases), "phases": phases,
         "analysis_format": analysis.get("format"), "analysis_phase_count": len(analysis.get("phases", [])),
         "analysis_diagnostics": [{"directory": phase.get("directory"), "diagnostics": phase.get("summary", {}).get("diagnostics"),
                                    "resource_trends_recomputed": phase.get("summary", {}).get("resource_trends_recomputed")}
                                   for phase in analysis.get("phases", [])],
         "supplementary_integrity": extra.get("integrity"), "supplementary_diagnostics": extra.get("diagnostics"),
         "supplementary_source_binding": extra_binding,
         "nested_gzip_failure_count": sum(item["nested_gzip_failure_count"] for item in verified),
         "nested_gzip_failures": [row for item in verified for row in item["nested_gzip_checks"] if not row["crc_eof_verified"]],
         "supplementary_phase_diagnostics": [{"name": phase.get("name"), "diagnostics": phase.get("supplementary_diagnostics")}
                                              for phase in extra.get("phases", [])],
         "metadata_diagnostics": diagnostics,
         "limitations": ["Controller, orchestration, phase failures and analyzer diagnostics remain as recorded; archival integrity is not workload success.",
             "Planned calibration is 3300-second shadows and five 600-second pressure phases per mode. Actual durations remain in phase records.",
             "The configured 3600-second warmup exceeds planned calibration phases; absent steady-state resource slopes are expected, not proof of stable resources.",
             "Supplementary historical-rate/capacity projections do not establish future bounds or choose acceptance thresholds.",
             "Upstream analysis has no per-input hashes; preserve its stated evidence-binding limitation and supplementary cross-checks.",
             "Historical extra-analysis smoke and RSS preflight are retained independently; they are not new tests run by this archiver.",
             "A damaged inner gzip is preserved byte-for-byte and reported as failed; verified outer/member hashes do not make its payload complete.",
             "Windows copies preserve bytes, not original Linux owners/modes. This script starts no WSL, load, tests, build or analyzer."]})
    save(output / "archive-verification.json", {"verified_utc": datetime.now(timezone.utc).isoformat(),
         "archives": verified, "all_originals_rehashed": True, "source_file_sets_unchanged": True,
         "direct_copies_sha256_size_match": True,
         "nested_gzip_failure_count": sum(item["nested_gzip_failure_count"] for item in verified),
         "method": "Per-original size/SHA inventory; safe temporary extraction of each stored member; strict outer gzip CRC/EOF; every inner gzip attempted with failures preserved/reported; original and direct-copy rehash."})
    for path in output.iterdir():
        raw = subprocess.check_output(["git", "hash-object", "--no-filters", str(path)], cwd=ROOT)
        filtered = subprocess.check_output(["git", "hash-object", "--path=" + path.relative_to(ROOT).as_posix(), str(path)], cwd=ROOT)
        if raw != filtered:
            raise RuntimeError(f"Git filters alter evidence: {path.name}")
    manifest = {"archived_utc": datetime.now(timezone.utc).isoformat(), "run_id": source.name,
         "scope": "calibration evidence only", "source_file_count": len(source_files), "archive_count": len(verified),
         "git_path_filters_preserve_bytes": True,
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
        raise RuntimeError("Git filters alter final manifest")
    print(json.dumps({"output": str(output), "archive_count": len(verified), "original_calibration_files": len(source_files),
                      "exact_binary_aliases": len(aliases), "controller_exit": controller_exit,
                      "orchestration_passed": layout.get("orchestration_passed"),
                      "scope": "calibration only", "manifest_sha256": sha256(manifest_path)}), flush=True)


if __name__ == "__main__":
    main()
