//! 使用官方 GitHub CLI 和内置信任根离线验证，禁止失败降级。
//! Verifies offline with the official GitHub CLI and embedded roots, without fallback.

use super::{Version, file_hash, private_tempdir};
use serde_json::Value;
use std::{
    fs, io,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const REPOSITORY: &str = "921108257/ling";
const WORKFLOW: &str = "https://github.com/921108257/ling/.github/workflows/release.yml";
const ROOT: &str = include_str!("trusted-root.json");

/// 外部验证器必须提供本版本依赖的全部策略标志。
/// The external verifier must support all policy flags required by this version.
pub(super) fn available() -> bool {
    let mut check = command();
    check.arg("--version");
    let Ok(output) = bounded_output(check, Duration::from_secs(5)) else {
        return false;
    };
    String::from_utf8_lossy(&output)
        .split_whitespace()
        .nth(2)
        .and_then(|value| Version::parse(value).ok())
        .is_some_and(|version| version >= Version::new(2, 97, 0))
}

/// 不继承用户的 GitHub 凭据；bundle 与固定根使验证完全离线。
/// Do not inherit GitHub credentials; the bundle and pinned root make verification offline.
fn command() -> Command {
    let mut command = Command::new("gh");
    for name in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "LING_GITHUB_TOKEN",
        "GH_DEBUG",
    ] {
        command.env_remove(name);
    }
    command
        .env("GH_HOST", "github.com")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
        .env("GH_TELEMETRY", "false");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

/// 有界等待与落盘输出，避免外部验证器占用无界内存或永不结束。
/// Bounds process time and on-disk output so the external verifier cannot hang or consume unbounded memory.
fn bounded_output(mut command: Command, timeout: Duration) -> io::Result<Vec<u8>> {
    let workspace = private_tempdir("ling-verifier-", None)?;
    let output = workspace.path().join("output.json");
    let mut child = command
        .env("GH_CONFIG_DIR", workspace.path())
        .stdin(Stdio::null())
        .stdout(fs::File::create(&output)?)
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline || fs::metadata(&output)?.len() > super::MAX_METADATA {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("provenance verifier exceeded its limit"));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !status.success() || fs::metadata(&output)?.len() > super::MAX_METADATA {
        return Err(io::Error::other("provenance verification failed"));
    }
    fs::read(output)
}

/// 先校验密码学与证书身份，再检查已验证声明中的资产名和摘要。
/// Checks cryptography and certificate identity before matching the verified subject name and digest.
pub(super) fn verify(package: &Path, bundle: &Path, version: &Version) -> io::Result<()> {
    verify_ref(package, bundle, &format!("refs/tags/v{version}"))
}

/// 共用完整身份策略；生产调用方只传入由目标版本构成的标签。
/// Shares the complete identity policy; production supplies only the target version's tag.
fn verify_ref(package: &Path, bundle: &Path, reference: &str) -> io::Result<()> {
    if !available() {
        return Err(io::Error::other("GitHub CLI 2.97.0 or later is required"));
    }
    let workspace = private_tempdir("ling-provenance-", None)?;
    let root = workspace.path().join("trusted-root.jsonl");
    fs::write(&root, ROOT)?;
    let mut verifier = command();
    verifier
        .args(["attestation", "verify"])
        .arg(package)
        .arg("--bundle")
        .arg(bundle)
        .args(["--repo", REPOSITORY, "--hostname", "github.com"])
        .args(["--cert-identity", &format!("{WORKFLOW}@{reference}")])
        .args(["--source-ref", reference])
        .args([
            "--cert-oidc-issuer",
            "https://token.actions.githubusercontent.com",
        ])
        .args([
            "--predicate-type",
            "https://slsa.dev/provenance/v1",
            "--deny-self-hosted-runners",
        ])
        .arg("--custom-trusted-root")
        .arg(root)
        .args(["--format", "json"]);
    let result: Value =
        serde_json::from_slice(&bounded_output(verifier, Duration::from_secs(60))?)?;
    let name = package
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("invalid asset name"))?;
    verified_subject(&result, name, &file_hash(package)?)
}

/// 只接受验证器成功输出的非空结果，未验证 bundle 的 payload 不能进入此入口。
/// Accepts nonempty successful verifier output; unverified bundle payloads never enter this path.
fn verified_subject(result: &Value, name: &str, digest: &str) -> io::Result<()> {
    let valid = result.as_array().is_some_and(|entries| {
        entries.iter().any(|entry| {
            let statement = &entry["verificationResult"]["statement"];
            statement["predicateType"] == "https://slsa.dev/provenance/v1"
                && statement["subject"].as_array().is_some_and(|subjects| {
                    subjects.iter().any(|subject| {
                        subject["name"] == name && subject["digest"]["sha256"] == digest
                    })
                })
        })
    });
    if valid {
        Ok(())
    } else {
        Err(io::Error::other("verified provenance subject mismatch"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CI 提供本工作流的真实产物；本地无产物时不要求联网或生成证明。
    /// CI supplies real workflow artifacts; local runs do not require networking or attestation creation.
    #[test]
    fn workflow_bundle_verifies_with_real_gh() {
        let Ok(directory) = std::env::var("LING_TEST_PROVENANCE_DIST") else {
            return;
        };
        let reference = std::env::var("LING_TEST_PROVENANCE_REF").unwrap();
        let directory = Path::new(&directory);
        let package = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.to_string_lossy().ends_with(".tar.gz"))
            .expect("CI must supply a real portable package");
        let bundle = directory.join("PROVENANCE.sigstore.json");
        verify_ref(&package, &bundle, &reference).unwrap();

        let workspace = private_tempdir("ling-provenance-contract-", None).unwrap();
        let renamed = workspace.path().join("different-package.tar.gz");
        fs::copy(&package, &renamed).unwrap();
        assert!(verify_ref(&renamed, &bundle, &reference).is_err());
        let modified = workspace.path().join(package.file_name().unwrap());
        let mut bytes = fs::read(&package).unwrap();
        bytes.push(0);
        fs::write(&modified, bytes).unwrap();
        assert!(verify_ref(&modified, &bundle, &reference).is_err());
        assert!(verify_ref(&package, &bundle, "refs/tags/v0.0.0-invalid").is_err());
        assert!(verify_ref(&package, &workspace.path().join("missing.json"), &reference).is_err());
    }

    /// 验证器返回成功后仍绑定精确名称与摘要；空、截断和不匹配结果拒绝。
    /// Even successful verifier output must bind the exact name and digest; malformed results fail.
    #[test]
    fn verified_statement_requires_exact_subject() {
        let result = serde_json::json!([{"verificationResult":{"statement":{
            "predicateType":"https://slsa.dev/provenance/v1",
            "subject":[{"name":"package.zip","digest":{"sha256":"abc"}}]
        }}}]);
        assert!(verified_subject(&result, "package.zip", "abc").is_ok());
        assert!(verified_subject(&result, "package.zip", "def").is_err());
        assert!(verified_subject(&result, "other.zip", "abc").is_err());
        assert!(verified_subject(&serde_json::json!([]), "package.zip", "abc").is_err());
        assert!(serde_json::from_slice::<Value>(b"[{\"verificationResult\":").is_err());
    }

    /// 内置根仅接受公共 Sigstore，保留多把有效/历史根用于平滑轮换。
    /// Embedded public Sigstore roots retain active and historical authorities for rotation.
    #[test]
    fn embedded_root_is_public_and_has_rotation_overlap() {
        let root: Value = serde_json::from_str(ROOT).unwrap();
        assert!(root["certificateAuthorities"].as_array().unwrap().len() >= 2);
        assert!(
            root["tlogs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|log| log["baseUrl"].as_str().unwrap().ends_with(".sigstore.dev"))
        );
    }
}
