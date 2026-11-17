#!/usr/bin/env python3
"""校验固定的 CI 输入，下载工具后先校验摘要；validate pinned CI inputs and downloaded tools."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import tomllib


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "packaging/build-inputs.json"


def load_manifest(path=MANIFEST):
    """版本、标签和资产名必须一致；require consistent versions, tags and asset names."""
    manifest = json.loads(path.read_text(encoding="utf-8"))
    for version in manifest["rust"].values():
        if not re.fullmatch(r"\d+\.\d+\.\d+", version):
            raise ValueError("Rust versions must include an exact patch version")
    for name, tool in manifest["tools"].items():
        expected_tag = ("v" if name == "nfpm" else "") + tool["version"]
        if tool["tag"] != expected_tag:
            raise ValueError(f"{name}: version and release tag differ")
        for arch, asset in tool["assets"].items():
            expected_name = (
                f"nfpm_{tool['version']}_Linux_{'arm64' if arch == 'aarch64' else arch}.tar.gz"
                if name == "nfpm" else f"appimagetool-{arch}.AppImage"
            )
            if asset["name"] != expected_name:
                raise ValueError(f"{name}: version and asset name differ")
            if not re.fullmatch(r"[0-9a-f]{64}", asset["sha256"]):
                raise ValueError(f"{name}: invalid SHA256")
    return manifest


def check_workflows(root, manifest):
    """本地工作流保持相对引用，外部动作只接受提交摘要；only external actions need commit pins."""
    rust_versions = set(manifest["rust"].values())
    for path in sorted((root / ".github/workflows").glob("*.y*ml")):
        source = path.read_text(encoding="utf-8")
        for match in re.finditer(r"(?m)^\s*(?:-\s*)?uses:\s*([^\s#]+)", source):
            action = match[1].strip("\"'")
            if action.startswith("./"):
                continue
            if not re.fullmatch(r"[^@]+@[0-9a-f]{40}", action):
                raise ValueError(f"{path.name}: action is not pinned: {action}")
        for match in re.finditer(
            r"uses:\s*dtolnay/rust-toolchain@[^\n]+\n\s+with:\n\s+toolchain:\s*([^\s#]+)",
            source,
        ):
            if match[1].strip("\"'") not in rust_versions:
                raise ValueError(f"{path.name}: Rust toolchain differs from build-inputs.json")
        toolchain_actions = len(re.findall(r"uses:\s*dtolnay/rust-toolchain@", source))
        explicit_toolchains = len(re.findall(r"(?m)^\s+toolchain:\s*", source))
        if toolchain_actions != explicit_toolchains:
            raise ValueError(f"{path.name}: Rust toolchain must be explicit")
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    rust_version = workspace["workspace"]["package"]["rust-version"]
    if rust_version.split(".")[:2] != manifest["rust"]["msrv"].split(".")[:2]:
        raise ValueError("Cargo rust-version differs from the CI MSRV")


def verify_file(path, expected):
    """流式验证仓内摘要，不依赖同源校验文件；hash against the repository pin, not a co-hosted checksum."""
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected:
        raise ValueError(f"SHA256 mismatch: {path.name}")


def download_tool(manifest, name, arch, directory):
    """摘要通过后才提供可用工具名；expose the usable tool name only after digest validation."""
    tool = manifest["tools"][name]
    asset = tool["assets"][arch]
    directory.mkdir(parents=True, exist_ok=True)
    subprocess.run([
        "gh", "release", "download", tool["tag"], "--repo", tool["repository"],
        "--pattern", asset["name"], "--dir", str(directory), "--clobber",
    ], check=True)
    source = directory / asset["name"]
    verify_file(source, asset["sha256"])
    source.replace(directory / tool["output"])


def main():
    """CI 检查与工具下载入口；entry point for CI validation and tool downloads."""
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check")
    download = commands.add_parser("download")
    download.add_argument("tool", choices=("nfpm", "appimagetool"))
    download.add_argument("arch", choices=("x86_64", "aarch64"))
    download.add_argument("directory", type=Path)
    args = parser.parse_args()
    manifest = load_manifest()
    if args.command == "check":
        check_workflows(ROOT, manifest)
        print("CI action, Rust and packaging-tool inputs are pinned")
    else:
        download_tool(manifest, args.tool, args.arch, args.directory)


if __name__ == "__main__":
    main()
