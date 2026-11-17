#!/usr/bin/env python3
"""记录更新器依赖与可选的独立构建体积；Measure updater dependencies and optional isolated release sizes."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess


def capture(command, output):
    """保留原始输出和精确参数，非零即失败；Retain raw output and exact arguments, failing on nonzero status."""
    result = subprocess.run(command, check=False, capture_output=True, text=True)
    output.write_text(result.stdout + result.stderr, encoding="utf-8")
    if result.returncode:
        raise RuntimeError(f"Command failed ({result.returncode}): {command!r}; inspect {output}")
    return result.stdout


def packages(tree):
    """以名称和版本去重，保留构建依赖；Deduplicate by crate name/version while retaining build dependencies."""
    return sorted({
        f"{match[1]} {match[2]}"
        for line in tree.splitlines()
        if (match := re.match(r"(\S+) v([^\s]+)", line)) and match[1] != "ling-server"
    })


def main():
    """默认只读清点；体积测量显式开启且使用两个独立目录。 / Inventory by default; size builds use distinct directories."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Rust target triple; defaults to the rustc host")
    parser.add_argument("--output", type=Path, default=Path("target/update-dependency-measurement"))
    parser.add_argument("--build", action="store_true", help="Build both feature configurations and measure ling")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    rustc = capture(["rustc", "-vV"], args.output / "rustc.txt")
    target = args.target or re.search(r"^host: (.+)$", rustc, re.MULTILINE)[1]
    revision = capture(["git", "rev-parse", "HEAD"], args.output / "revision.txt").strip()
    status = capture(["git", "status", "--porcelain"], args.output / "worktree.txt")
    report = {
        "timestamp_utc": datetime.now(timezone.utc).isoformat(),
        "os": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "target": target,
        "revision": revision,
        "worktree_dirty": bool(status.strip()),
        "rustc": rustc,
        "rustflags": os.environ.get("RUSTFLAGS", ""),
        "cargo_encoded_rustflags": os.environ.get("CARGO_ENCODED_RUSTFLAGS", ""),
        "counting": "Unique crate name/version pairs in normal+build edges, excluding ling-server; includes workspace dependencies and build-time crates; no dev edges.",
        "audit": "not executed by this script; use cargo audit --json and retain tool/database revisions separately",
        "configurations": {},
    }
    graphs = {}
    for name, features in [("default", []), ("without-self-update", ["--no-default-features"])]:
        tree_command = ["cargo", "tree", "--locked", "-p", "ling-server", "-e", "normal,build", "--target", target, "--prefix", "none", "--format", "{p}", *features]
        tree = capture(tree_command, args.output / f"{name}-tree.txt")
        graphs[name] = set(packages(tree))
        measured = {"tree_command": tree_command, "dependency_count": len(graphs[name]), "dependencies": sorted(graphs[name]), "binary": "not measured (--build was not specified)"}
        if args.build:
            build_dir = (args.output / f"build-{name}").resolve()
            command = ["cargo", "build", "--locked", "--release", "--target", target, "--target-dir", str(build_dir), "-p", "ling-server", "--bin", "ling", *features]
            capture(command, args.output / f"{name}-build.txt")
            binary = build_dir / target / "release" / ("ling.exe" if "windows" in target else "ling")
            with binary.open("rb") as source:
                digest = hashlib.file_digest(source, "sha256").hexdigest()
            measured["binary"] = {"command": command, "bytes": binary.stat().st_size, "sha256": digest}
        report["configurations"][name] = measured
        # 即使后续构建失败，也不丢失已经完成的清点数据。
        # Persist completed measurements even when a later build fails.
        (args.output / "measurement.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    report["updater_only_dependencies"] = sorted(graphs["default"] - graphs["without-self-update"])
    report["updater_only_count"] = len(report["updater_only_dependencies"])
    (args.output / "measurement.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
