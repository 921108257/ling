#!/usr/bin/env python3
"""真实离线验签及篡改契约；Verify real offline attestations and adversarial inputs."""

import argparse
import hashlib
import json
import os
import re
import subprocess
import tempfile
from pathlib import Path


def checksums(path, selected=None):
    """摘要清单只允许唯一文件名；Accept unique plain filenames and require every selected asset."""
    sums = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        parts = line.split("  ", 1)
        if len(parts) != 2:
            raise ValueError("Invalid checksum line")
        digest, name = parts
        if not re.fullmatch(r"[0-9a-f]{64}", digest) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name):
            raise ValueError("Invalid checksum digest or filename")
        if name in sums:
            raise ValueError(f"Duplicate checksum asset: {name}")
        sums[name] = digest
    names = list(sums) if selected is None else selected
    if not names or len(set(names)) != len(names):
        raise ValueError("Select at least one unique asset")
    for name in names:
        if name not in sums:
            raise ValueError(f"Selected asset missing from checksums: {name}")
        if not (path.parent / name).is_file():
            raise ValueError(f"Selected asset missing from disk: {name}")
    return {name: sums[name] for name in names}


def source_digest(value):
    """来源 commit 必须完整且无 shell 字符；Require a complete lowercase source commit."""
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise argparse.ArgumentTypeError("Source digest must be a full lowercase commit")
    return value


def main():
    """独立验证每份资产；失败路径始终保留隔离安装标记。Verify all subjects and preserve install sentinels."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--ref", required=True)
    parser.add_argument("--source-digest", type=source_digest)
    parser.add_argument("--asset", action="append", help="Verify only this exact asset; may be repeated")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    dist = args.dist.resolve()
    bundle = dist / "PROVENANCE.sigstore.json"
    trusted = root / "crates/server/src/update/trusted-root.json"
    with tempfile.TemporaryDirectory(prefix="ling-provenance-test-") as work:
        work = Path(work)
        installed = work / "installed"
        installed.write_bytes(b"unchanged installation")
        environment = {k: v for k, v in os.environ.items() if k not in {
            "GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "LING_GITHUB_TOKEN", "GH_DEBUG"}}
        environment.update(GH_HOST="github.com", GH_CONFIG_DIR=str(work), GH_PROMPT_DISABLED="1",
                           GH_NO_UPDATE_NOTIFIER="1", GH_NO_EXTENSION_UPDATE_NOTIFIER="1", GH_TELEMETRY="false",
                           HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9", NO_PROXY="")

        def verify(path, proof=bundle, ref=args.ref, repo="921108257/ling", digest=args.source_digest, success=True):
            """调用官方验证器，证书、来源ref、托管runner与内置root策略同时成立。"""
            command = ["gh", "attestation", "verify", str(path), "--bundle", str(proof),
                       "--custom-trusted-root", str(trusted), "--repo", repo,
                       "--hostname", "github.com", "--cert-identity",
                       f"https://github.com/921108257/ling/.github/workflows/release.yml@{ref}",
                       "--source-ref", ref, "--cert-oidc-issuer", "https://token.actions.githubusercontent.com",
                       "--predicate-type", "https://slsa.dev/provenance/v1", "--deny-self-hosted-runners",
                       "--format", "json"]
            if digest is not None:
                command.extend(["--source-digest", digest])
            result = subprocess.run(command, env=environment, capture_output=True,
                                    text=True, encoding="utf-8", timeout=60)
            assert (result.returncode == 0) == success, result.stderr
            if success:
                verified = json.loads(result.stdout)
                assert any(subject["name"] == path.name and subject["digest"]["sha256"] == hashlib.sha256(path.read_bytes()).hexdigest()
                           for entry in verified for subject in entry["verificationResult"]["statement"]["subject"])
            assert installed.read_bytes() == b"unchanged installation"

        # 先信任签名后的清单，再读取其中的文件名和摘要。
        # Authenticate the checksum manifest before trusting its filenames and digests.
        verify(dist / "SHA256SUMS")
        sums = checksums(dist / "SHA256SUMS", args.asset)
        for name, digest in sums.items():
            asset = dist / name
            assert hashlib.sha256(asset.read_bytes()).hexdigest() == digest, name
            verify(asset)
        asset = dist / next(iter(sums))
        # 篡改资产与同时伪造的校验和仍不得通过来源证明。
        # Changing both the artifact and its untrusted checksum must still fail provenance verification.
        tampered = work / asset.name
        tampered.write_bytes(asset.read_bytes() + b"tampered")
        forged_checksum = hashlib.sha256(tampered.read_bytes()).hexdigest()
        assert forged_checksum != sums[asset.name]
        verify(tampered, success=False)
        proof = work / "truncated.json"
        proof.write_bytes(bundle.read_bytes()[:128])
        verify(asset, proof=proof, success=False)
        proof.write_text("{}", encoding="utf-8")
        verify(asset, proof=proof, success=False)
        verify(asset, proof=work / "missing.json", success=False)
        verify(asset, repo="not-the-owner/ling", success=False)
        verify(asset, ref="refs/tags/v0.0.1", success=False)
        adversarial = 6
        if args.source_digest is not None:
            wrong_digest = "0" * 40 if args.source_digest != "0" * 40 else "1" * 40
            verify(asset, digest=wrong_digest, success=False)
            adversarial += 1
        print(f"Offline provenance verified for {len(sums) + 1} subjects; {adversarial} adversarial checks passed")


if __name__ == "__main__":
    main()
