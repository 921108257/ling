#!/usr/bin/env python3
"""真实离线验签及篡改契约；Verify real offline attestations and adversarial inputs."""

import argparse
import hashlib
import json
import os
import subprocess
import tempfile
from pathlib import Path


def main():
    """独立验证每份资产；失败路径始终保留隔离安装标记。Verify all subjects and preserve install sentinels."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--ref", required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    dist = args.dist.resolve()
    bundle = dist / "PROVENANCE.sigstore.json"
    trusted = root / "crates/server/src/update/trusted-root.json"
    sums = dict((name, digest) for digest, name in
                (line.split("  ", 1) for line in (dist / "SHA256SUMS").read_text().splitlines()))
    with tempfile.TemporaryDirectory(prefix="ling-provenance-test-") as work:
        work = Path(work)
        installed = work / "installed"
        installed.write_bytes(b"unchanged installation")
        environment = {k: v for k, v in os.environ.items() if k not in {
            "GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "LING_GITHUB_TOKEN", "GH_DEBUG"}}
        environment.update(GH_HOST="github.com", GH_CONFIG_DIR=str(work), GH_PROMPT_DISABLED="1",
                           GH_NO_UPDATE_NOTIFIER="1", GH_NO_EXTENSION_UPDATE_NOTIFIER="1", GH_TELEMETRY="false",
                           HTTP_PROXY="http://127.0.0.1:9", HTTPS_PROXY="http://127.0.0.1:9", NO_PROXY="")

        def verify(path, proof=bundle, ref=args.ref, repo="921108257/ling", success=True):
            """调用官方验证器，证书、来源ref、托管runner与内置root策略同时成立。"""
            result = subprocess.run(["gh", "attestation", "verify", str(path), "--bundle", str(proof),
                                     "--custom-trusted-root", str(trusted), "--repo", repo,
                                     "--hostname", "github.com", "--cert-identity",
                                     f"https://github.com/921108257/ling/.github/workflows/release.yml@{ref}",
                                     "--source-ref", ref, "--cert-oidc-issuer", "https://token.actions.githubusercontent.com",
                                     "--predicate-type", "https://slsa.dev/provenance/v1", "--deny-self-hosted-runners",
                                     "--format", "json"], env=environment, capture_output=True,
                                    text=True, encoding="utf-8", timeout=60)
            assert (result.returncode == 0) == success, result.stderr
            if success:
                verified = json.loads(result.stdout)
                assert any(subject["name"] == path.name and subject["digest"]["sha256"] == hashlib.sha256(path.read_bytes()).hexdigest()
                           for entry in verified for subject in entry["verificationResult"]["statement"]["subject"])
            assert installed.read_bytes() == b"unchanged installation"

        for name, digest in sums.items():
            asset = dist / name
            assert hashlib.sha256(asset.read_bytes()).hexdigest() == digest, name
            verify(asset)
        verify(dist / "SHA256SUMS")
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
        print(f"Offline provenance verified for {len(sums) + 1} subjects; six adversarial checks passed")


if __name__ == "__main__":
    main()
