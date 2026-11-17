#!/usr/bin/env python3
"""为每个发行资产生成 CycloneDX 1.6 清单；Generate per-asset CycloneDX 1.6 inventories."""

import argparse
import hashlib
import json
import re
import subprocess
import tomllib
from pathlib import Path


def cargo(*args):
    """读取与发行构建相同的锁定依赖图；Read the locked release dependency graph."""
    return subprocess.check_output(["cargo", *args], text=True, encoding="utf-8")


def spdx_expression(license_expression):
    """Cargo 旧斜线分隔符表示可选许可；Cargo's legacy slash separator means SPDX OR."""
    return re.sub(r"\s*/\s*", " OR ", license_expression)


def main():
    """仅登记目标平台的普通/构建依赖，不包含开发依赖；Omit dev-only dependencies."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    args = parser.parse_args()
    metadata = json.loads(cargo("metadata", "--locked", "--format-version", "1"))
    graph = cargo("tree", "--locked", "-p", "ling-server", "-p", "ling-cli", "-e", "normal,build",
                  "--target", args.target, "--prefix", "none", "--format", "{p}")
    selected = set(re.findall(r"^([^ ]+) v(\S+)", graph, re.MULTILINE))
    checksums = {(p["name"], p["version"]): p["checksum"]
                 for p in tomllib.loads(Path("Cargo.lock").read_text())["package"] if "checksum" in p}
    components = []
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
        identity = (package["name"], package["version"])
        if identity not in selected:
            continue
        ref = f"pkg:cargo/{package['name']}@{package['version']}"
        component = {"type": "library", "bom-ref": ref, "name": package["name"],
                     "version": package["version"], "purl": ref}
        if package["license"]:
            component["licenses"] = [{"expression": spdx_expression(package["license"])}]
        if identity in checksums:
            component["hashes"] = [{"alg": "SHA-256", "content": checksums[identity]}]
        components.append(component)
    if not components:
        raise RuntimeError("Release dependency inventory is empty")
    assets = sorted(path for path in args.dist.iterdir()
                    if path.is_file() and not path.name.endswith(".cdx.json"))
    if not assets:
        raise RuntimeError("No release assets found")
    for asset in assets:
        inventory = list(components)
        if asset.name.endswith("_windows_x86_64_setup.exe"):
            inventory.append({"type": "application", "bom-ref": "WinSW-2.12.0", "name": "WinSW",
                              "version": "2.12.0", "licenses": [{"license": {"id": "MIT"}}],
                              "hashes": [{"alg": "SHA-256", "content":
                                          "05b82d46ad331cc16bdc00de5c6332c1ef818df8ceefcd49c726553209b3a0da"}],
                              "externalReferences": [{"type": "distribution", "url":
                                  "https://github.com/winsw/winsw/releases/download/v2.12.0/WinSW-x64.exe"}]})
        root = {"type": "application", "bom-ref": asset.name, "name": "ling", "version": args.version,
                "hashes": [{"alg": "SHA-256", "content": hashlib.sha256(asset.read_bytes()).hexdigest()}],
                "licenses": [{"license": {"id": "MIT"}}],
                "properties": [{"name": "ling:target", "value": args.target},
                               {"name": "ling:inventory-scope", "value": "Cargo normal/build dependencies; includes build tools"}]}
        bom = {"$schema": "https://cyclonedx.org/schema/bom-1.6.schema.json", "bomFormat": "CycloneDX",
               "specVersion": "1.6", "version": 1, "metadata": {"component": root}, "components": inventory}
        path = asset.with_name(asset.name + ".cdx.json")
        path.write_text(json.dumps(bom, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(f"SBOM: {path.name}: {len(inventory)} components")


if __name__ == "__main__":
    main()
