#!/usr/bin/env python3
"""按固定资产矩阵校验清单；Validate the fixed package matrix and provenance attachments."""

import argparse
import hashlib
import json
from pathlib import Path


def package_names(version):
    """与既有发行名称保持一致；Keep established package names unchanged."""
    names = set()
    for arch in ("x86_64", "aarch64"):
        names.update(f"ling_{version}_linux_{arch}.{ext}" for ext in ("deb", "rpm", "tar.gz", "AppImage"))
        names.update(f"ling_{version}_macos_{arch}.{ext}" for ext in ("pkg", "tar.gz"))
    names.add(f"ling_{version}_windows_x86_64.zip")
    names.add(f"ling_{version}_windows_x86_64_setup.exe")
    return names


def main():
    """先生成校验和，签名后再次拒绝缺失/额外附件；Reject missing or unexpected release files."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    parser.add_argument("--final", action="store_true")
    args = parser.parse_args()
    packages = package_names(args.version)
    subjects = packages | {name + ".cdx.json" for name in packages} | {"dependency-audit.json"}
    expected = subjects | ({"SHA256SUMS", "PROVENANCE.sigstore.json"} if args.final else set())
    actual = {path.name for path in args.dist.iterdir()}
    if expected != actual:
        raise RuntimeError(f"Release assets mismatch: missing {sorted(expected - actual)}, extra {sorted(actual - expected)}")
    for name in packages:
        bom = json.loads((args.dist / (name + ".cdx.json")).read_text(encoding="utf-8"))
        assert bom["bomFormat"] == "CycloneDX" and bom["specVersion"] == "1.6"
        assert bom["metadata"]["component"]["hashes"][0]["content"] == hashlib.sha256((args.dist / name).read_bytes()).hexdigest()
        assert bom["components"], f"Empty SBOM: {name}"
        if name.endswith("_setup.exe"):
            assert any(c["name"] == "WinSW" for c in bom["components"]), "WinSW missing"
    sums = "".join(f"{hashlib.sha256((args.dist / name).read_bytes()).hexdigest()}  {name}\n" for name in sorted(subjects))
    checksum_path = args.dist / "SHA256SUMS"
    if args.final:
        assert checksum_path.read_text(encoding="utf-8") == sums, "Checksum manifest changed"
        assert json.loads((args.dist / "PROVENANCE.sigstore.json").read_text(encoding="utf-8")), "Empty bundle"
    else:
        checksum_path.write_text(sums, encoding="utf-8", newline="\n")
    print(f"Verified {len(packages)} packages, {len(packages)} SBOMs, and audit report" + (", checksums and provenance" if args.final else ""))


if __name__ == "__main__":
    main()
