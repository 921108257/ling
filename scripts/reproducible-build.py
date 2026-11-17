#!/usr/bin/env python3
"""比较同提交的两次独立构建，不承诺逐字节可复现；Compare independent builds without promising reproducibility."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import subprocess


def capture(command, source_root, output, env=None):
    """保留命令输出，真实构建失败仍应使 CI 失败；Keep logs and fail CI for actual build failures."""
    with output.open("w", encoding="utf-8") as log:
        result = subprocess.run(command, cwd=source_root, env=env, stdout=log, stderr=subprocess.STDOUT, text=True)
    if result.returncode:
        raise RuntimeError(f"Command failed ({result.returncode}); inspect {output}")
    return output.read_text(encoding="utf-8")


def build_flags(environment, source_root, target_dir, remap_root):
    """保留原始标志，统一源码与输出路径；Preserve original flags and normalize source/output paths."""
    encoded = environment.get("CARGO_ENCODED_RUSTFLAGS")
    flags = encoded.split("\x1f") if encoded else shlex.split(environment.get("RUSTFLAGS", ""))
    return flags + [
        f"--remap-path-prefix={source_root}={remap_root}/src",
        f"--remap-path-prefix={target_dir}={remap_root}/target",
    ]


def binary_record(path):
    """记录完整二进制摘要和大小；Record whole-binary digests and sizes."""
    with path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    return {"sha256": digest, "bytes": path.stat().st_size}


def compare(first, second):
    """差异是测量结果，不能伪装成构建错误；Differences are observations, not build errors."""
    return {
        name: {"same_sha256": first[name]["sha256"] == second[name]["sha256"],
               "same_bytes": first[name]["bytes"] == second[name]["bytes"]}
        for name in first
    }


def main():
    """只接受干净提交，写入独立输出目录；Use a clean commit and separate output directories only."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output", type=Path, default=Path(os.environ.get("RUNNER_TEMP", "target")) / "ling-reproducibility")
    parser.add_argument("--target", help="Rust target triple; defaults to the compiler host")
    parser.add_argument("--remap-root", default="/ling", help="Virtual prefix shared by both builds")
    args = parser.parse_args()
    source_root = args.source_root.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    revision = capture(["git", "rev-parse", "HEAD"], source_root, output / "revision.txt").strip()
    status = capture(["git", "status", "--porcelain"], source_root, output / "worktree-before.txt")
    if status.strip():
        raise RuntimeError("Reproducibility measurement requires a clean source checkout")
    timestamp = capture(["git", "show", "-s", "--format=%ct", "HEAD"], source_root, output / "source-date-epoch.txt").strip()
    rustc = capture(["rustc", "-vV"], source_root, output / "rustc.txt")
    cargo = capture(["cargo", "--version"], source_root, output / "cargo.txt")
    target = args.target or next(line.removeprefix("host: ") for line in rustc.splitlines() if line.startswith("host: "))
    report = {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "revision": revision,
        "os": platform.platform(),
        "machine": platform.machine(),
        "target": target,
        "rustc": rustc,
        "cargo": cargo,
        "source_date_epoch": timestamp,
        "remap_root": args.remap_root,
        "builds": [],
        "limitation": "Two independent target directories on one runner and one clean source commit; equal outputs do not establish reproducibility on other hosts or toolchains. Differences do not fail this job.",
    }
    for label in ("a", "b"):
        target_dir = output / ("build-" + label)
        target_dir.mkdir(exist_ok=False)
        env = os.environ.copy()
        flags = build_flags(env, source_root, target_dir, args.remap_root)
        env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
        env.pop("RUSTFLAGS", None)
        env["CARGO_INCREMENTAL"] = "0"
        env["SOURCE_DATE_EPOCH"] = timestamp
        command = ["cargo", "build", "--locked", "--release", "--target", target, "--target-dir", str(target_dir), "-p", "ling-server", "-p", "ling-cli"]
        capture(command, source_root, output / f"build-{label}.txt", env)
        suffix = ".exe" if "windows" in target else ""
        binaries = {name: binary_record(target_dir / target / "release" / (name + suffix)) for name in ("ling", "ling-cli")}
        report["builds"].append({"label": label, "command": command, "rustflags": flags, "binaries": binaries})
        (output / "measurement.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    report["comparison"] = compare(report["builds"][0]["binaries"], report["builds"][1]["binaries"])
    final_revision = capture(["git", "rev-parse", "HEAD"], source_root, output / "revision-after.txt").strip()
    final_status = capture(["git", "status", "--porcelain"], source_root, output / "worktree-after.txt")
    report["source_unchanged"] = final_revision == revision and final_status == status
    (output / "measurement.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    if not report["source_unchanged"]:
        raise RuntimeError("Source changed during measurement; comparison is invalid")
    print(json.dumps(report["comparison"], indent=2))
    print(f"Evidence: {output / 'measurement.json'}")


if __name__ == "__main__":
    main()
