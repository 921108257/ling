//! Windows 更新交接；父进程退出后才由隐藏帮助进程应用已验证产物。
//! Windows update handoff; a hidden helper applies verified artifacts only after the parent exits.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// 固定模板与 JSON 参数分离，不把用户路径插入 PowerShell 源码。
/// Separates the fixed script from JSON parameters, never interpolating paths into PowerShell source.
const SCRIPT: &str = include_str!("windows.ps1");

/// 应用计划不包含网络凭据，目标始终是当前安装目录。
/// The apply plan contains no network credentials and always targets the current installation directory.
#[derive(Serialize)]
struct Plan {
    parent_pid: u32,
    current: PathBuf,
    expected_current_sha256: String,
    lock_path: PathBuf,
    installer: Option<Installer>,
    binaries: Vec<Binary>,
}

/// 原生安装器与固定安装根；Native installer and fixed installation root.
#[derive(Serialize)]
struct Installer {
    source: PathBuf,
    sha256: String,
    install_dir: PathBuf,
}

/// 便携二进制与预先计算的摘要；Portable binary and its precomputed digest.
#[derive(Serialize)]
struct Binary {
    source: PathBuf,
    target: PathBuf,
    sha256: String,
}

/// 持久化计划并启动隐藏帮助进程；返回结果文件，不声明更新已经完成。
/// Persists a plan and launches a hidden helper; returns the result file without claiming completion.
pub(super) fn schedule(
    workspace: &Path,
    current: &Path,
    expected_current_sha256: &str,
    installer: Option<(&Path, &str)>,
    binaries: &[(PathBuf, PathBuf)],
) -> io::Result<PathBuf> {
    let plan = prepare(
        workspace,
        current,
        expected_current_sha256,
        installer,
        binaries,
        std::process::id(),
    )?;
    launch(&plan)?;
    Ok(plan.with_file_name("result.json"))
}

/// 来源必须位于本次工作目录，目标只能是当前安装的两个二进制。
/// Sources must belong to this update workspace and targets are limited to the current installation's two binaries.
fn prepare(
    workspace: &Path,
    current: &Path,
    expected_current_sha256: &str,
    installer: Option<(&Path, &str)>,
    binaries: &[(PathBuf, PathBuf)],
    parent_pid: u32,
) -> io::Result<PathBuf> {
    let workspace = fs::canonicalize(workspace)?;
    let current = fs::canonicalize(current)?;
    let directory = current.parent().ok_or_else(invalid)?;
    if expected_current_sha256.len() != 64
        || !expected_current_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid());
    }
    let installer = installer
        .map(|(source, hash)| {
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid());
            }
            let source = fs::canonicalize(source)?;
            if !source.starts_with(&workspace)
                || !directory
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("bin"))
            {
                return Err(invalid());
            }
            let install_dir = ordinary_path(directory.parent().ok_or_else(invalid)?)?;
            Ok(Installer {
                source: ordinary_path(&source)?,
                sha256: hash.to_ascii_lowercase(),
                install_dir,
            })
        })
        .transpose()?;
    if installer.is_none() && binaries.is_empty() {
        return Err(invalid());
    }
    // 原生安装目录可能只读，在用户临时目录按安装身份互斥。
    // Native install directories may be read-only; serialize by installation identity in user temporary storage.
    let lock_path = if installer.is_some() {
        let identity = ordinary_path(directory)?.to_string_lossy().to_lowercase();
        workspace.parent().ok_or_else(invalid)?.join(format!(
            ".ling-update-{:x}.lock",
            Sha256::digest(identity.as_bytes())
        ))
    } else {
        directory.join(".ling-update.lock")
    };
    let mut prepared = Vec::new();
    for (source, target) in binaries {
        let source = fs::canonicalize(source)?;
        let parent = target.parent().ok_or_else(invalid)?;
        let name = target.file_name().ok_or_else(invalid)?;
        if !source.starts_with(&workspace)
            || fs::canonicalize(parent)? != directory
            || !(name.to_string_lossy().eq_ignore_ascii_case("ling.exe")
                || name.to_string_lossy().eq_ignore_ascii_case("ling-cli.exe"))
            || prepared.iter().any(|binary: &Binary| {
                binary.target.file_name().is_some_and(|prior| {
                    prior
                        .to_string_lossy()
                        .eq_ignore_ascii_case(&name.to_string_lossy())
                })
            })
        {
            return Err(invalid());
        }
        prepared.push(Binary {
            sha256: digest(&source)?,
            source: ordinary_path(&source)?,
            target: ordinary_path(&directory.join(name))?,
        });
    }
    let plan = workspace.join("plan.json");
    fs::write(
        &plan,
        serde_json::to_vec(&Plan {
            parent_pid,
            current: ordinary_path(&current)?,
            expected_current_sha256: expected_current_sha256.to_ascii_lowercase(),
            lock_path: ordinary_path(&lock_path)?,
            installer,
            binaries: prepared,
        })
        .map_err(io::Error::other)?,
    )?;
    fs::write(workspace.join("apply.ps1"), SCRIPT)?;
    fs::write(
        workspace.join("result.json"),
        b"{\"status\":\"scheduled\",\"code\":null,\"phase\":\"handoff\"}\n",
    )?;
    ordinary_path(&plan)
}

/// PowerShell 5 的脚本入口不接受 canonicalize 产生的扩展前缀。
/// PowerShell 5's script entry point does not accept the extended prefix produced by canonicalize.
fn ordinary_path(path: &Path) -> io::Result<PathBuf> {
    let text = path.to_str().ok_or_else(invalid)?;
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        Ok(PathBuf::from(format!(r"\\{rest}")))
    } else {
        Ok(PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text)))
    }
}

/// 流式计算二进制摘要，不分配完整文件。
/// Computes the binary digest incrementally without allocating the entire file.
fn digest(path: &Path) -> io::Result<String> {
    let mut source = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// 使用系统 PowerShell，隐藏窗口并将固定脚本日志保留在工作目录。
/// Uses system PowerShell with a hidden window, retaining fixed-script logs in the workspace.
fn launch(plan: &Path) -> io::Result<Child> {
    let system = std::env::var_os("SystemRoot").ok_or_else(invalid)?;
    let shell = PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let log = File::create(plan.with_file_name("helper.log"))?;
    let mut command = Command::new(shell);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(plan.with_file_name("apply.ps1"))
        .arg("-PlanPath")
        .arg(plan)
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    command.spawn()
}

/// 不回显路径的本地参数错误；Local argument failure without echoing paths.
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "Invalid Windows update plan")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::windows::fs::OpenOptionsExt,
        time::{Duration, Instant},
    };

    /// 在独立临时目录使用文本文件模拟二进制，绝不运行真实安装器。
    /// Simulates binaries with text files in isolated temporary directories, never running an installer.
    fn fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        PathBuf,
        Vec<(PathBuf, PathBuf)>,
    ) {
        let workspace = tempfile::Builder::new()
            .prefix("ling update ' & 中文-")
            .tempdir()
            .unwrap();
        let install = tempfile::Builder::new()
            .prefix("ling install ' & 中文-")
            .tempdir()
            .unwrap();
        let mut binaries = Vec::new();
        for name in ["ling.exe", "ling-cli.exe"] {
            let source = workspace.path().join(name);
            let target = install.path().join(name);
            fs::write(&source, format!("new {name}")).unwrap();
            fs::write(&target, format!("old {name}")).unwrap();
            binaries.push((source, target));
        }
        let current = binaries[0].1.clone();
        (workspace, install, current, binaries)
    }

    /// 等待帮助进程写出最终结果，超时不杀未知进程。
    /// Waits for the helper's terminal result without killing unknown processes on timeout.
    fn result(plan: &Path, child: &mut Child) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = child.try_wait().unwrap();
            if let Some(value) = read_result(plan) {
                if matches!(
                    value["status"].as_str(),
                    Some("success" | "failed" | "unknown")
                ) {
                    child.wait().unwrap();
                    return value;
                }
            }
            assert!(
                status.is_none(),
                "helper exited {status:?} before a result: {}",
                String::from_utf8_lossy(
                    &fs::read(plan.with_file_name("helper.log")).unwrap_or_default()
                )
            );
            assert!(
                Instant::now() < deadline,
                "helper timed out: {}",
                fs::read_to_string(plan.with_file_name("helper.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 原子替换的短暂共享冲突可重试，不忽略其他读取或 JSON 错误。
    /// Retries transient sharing conflicts during atomic replacement without hiding other read or JSON failures.
    fn read_result(plan: &Path) -> Option<serde_json::Value> {
        match fs::read(plan.with_file_name("result.json")) {
            Ok(bytes) => Some(serde_json::from_slice(&bytes).unwrap()),
            Err(error) if matches!(error.raw_os_error(), Some(2 | 32)) => None,
            Err(error) => panic!("result read failed: {error}"),
        }
    }

    /// 便携更新仅替换发行二进制，不修改配置。
    /// Portable updates replace only distribution binaries and preserve configuration.
    #[test]
    fn portable_apply_preserves_configuration() {
        let (workspace, install, current, binaries) = fixture();
        fs::write(install.path().join("ling.toml"), "user config").unwrap();
        let plan = prepare(
            workspace.path(),
            &current,
            &digest(&current).unwrap(),
            None,
            &binaries,
            0,
        )
        .unwrap();
        let mut child = launch(&plan).unwrap();
        let completed = result(&plan, &mut child);
        assert_eq!(
            completed["status"],
            "success",
            "{completed} {}",
            String::from_utf8_lossy(
                &fs::read(plan.with_file_name("helper.log")).unwrap_or_default()
            )
        );
        for (source, target) in binaries {
            assert_eq!(fs::read(source).unwrap(), fs::read(target).unwrap());
        }
        assert_eq!(
            fs::read_to_string(install.path().join("ling.toml")).unwrap(),
            "user config"
        );
    }

    /// 校验失败不触及目标；第二文件被锁时撤销第一文件的替换。
    /// Hash failures leave targets intact; a locked second binary rolls back the first replacement.
    #[test]
    fn portable_hash_failure_and_file_lock_preserve_old_files() {
        for tamper in [true, false] {
            let (workspace, _install, current, binaries) = fixture();
            let plan = prepare(
                workspace.path(),
                &current,
                &digest(&current).unwrap(),
                None,
                &binaries,
                0,
            )
            .unwrap();
            let locked = if tamper {
                fs::write(&binaries[1].0, "changed after schedule").unwrap();
                None
            } else {
                Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(0)
                        .open(&binaries[1].1)
                        .unwrap(),
                )
            };
            let mut child = launch(&plan).unwrap();
            let completed = result(&plan, &mut child);
            assert_eq!(completed["status"], "failed", "{completed}");
            assert_eq!(
                completed["code"],
                if tamper {
                    "LOCAL_UPDATE_CHECKSUM"
                } else {
                    "LOCAL_UPDATE_BUSY"
                }
            );
            assert_eq!(completed["rollback_complete"], true);
            drop(locked);
            for (_, target) in binaries {
                assert!(fs::read_to_string(target).unwrap().starts_with("old "));
            }
        }
    }

    /// 排他目录锁阻止另一更新，模板没有强杀或动态源码执行。
    /// An exclusive directory lock blocks another update; the template neither kills processes nor evaluates dynamic source.
    #[test]
    fn portable_directory_lock_prevents_concurrent_apply() {
        let (workspace, install, current, binaries) = fixture();
        let plan = prepare(
            workspace.path(),
            &current,
            &digest(&current).unwrap(),
            None,
            &binaries,
            0,
        )
        .unwrap();
        let _locked = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .share_mode(0)
            .open(install.path().join(".ling-update.lock"))
            .unwrap();
        let mut child = launch(&plan).unwrap();
        let value = result(&plan, &mut child);
        assert_eq!(value["status"], "failed");
        assert_eq!(value["code"], "LOCAL_UPDATE_BUSY");
        assert!(!SCRIPT.contains("Stop-Process"));
        assert!(!SCRIPT.contains("Invoke-Expression"));
    }

    /// 下载期间或计划生成后安装内容变化时，旧计划不得覆盖任何目标。
    /// A changed installation during downloading or after planning makes stale updates leave all targets intact.
    #[test]
    fn changed_current_executable_rejects_stale_portable_plans() {
        for change_before_plan in [true, false] {
            let (workspace, install, current, binaries) = fixture();
            let expected = digest(&current).unwrap();
            if change_before_plan {
                fs::write(&current, "another update").unwrap();
            }
            let plan = prepare(workspace.path(), &current, &expected, None, &binaries, 0).unwrap();
            if !change_before_plan {
                fs::write(&current, "another update").unwrap();
            }
            let mut child = launch(&plan).unwrap();
            let completed = result(&plan, &mut child);
            assert_eq!(completed["status"], "failed");
            assert_eq!(completed["code"], "LOCAL_UPDATE_BUSY");
            assert_eq!(completed["phase"], "current-check");
            assert_eq!(fs::read_to_string(&current).unwrap(), "another update");
            assert_eq!(
                fs::read_to_string(&binaries[1].1).unwrap(),
                "old ling-cli.exe"
            );
            assert_eq!(fs::read_dir(install.path()).unwrap().count(), 3);
        }
    }

    /// 原生路径也先互斥和核对磁盘快照，失败时绝不启动占位安装器。
    /// Native paths also lock and verify disk snapshots before ever starting the placeholder installer.
    #[test]
    fn native_lock_and_stale_snapshot_fail_before_starting_installer() {
        let workspace = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let bin = install.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let current = bin.join("ling.exe");
        fs::write(&current, "old native").unwrap();
        let installer = workspace.path().join("not-an-installer.exe");
        fs::write(&installer, "must never be executed").unwrap();
        let expected = digest(&current).unwrap();
        let installer_hash = digest(&installer).unwrap();
        let plan = prepare(
            workspace.path(),
            &current,
            &expected,
            Some((&installer, &installer_hash)),
            &[],
            0,
        )
        .unwrap();
        let contents: serde_json::Value =
            serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();
        let lock_path = PathBuf::from(contents["lock_path"].as_str().unwrap());
        let second_workspace = tempfile::tempdir().unwrap();
        let second_installer = second_workspace.path().join("not-an-installer.exe");
        fs::write(&second_installer, "must never be executed").unwrap();
        let second_plan = prepare(
            second_workspace.path(),
            &current,
            &expected,
            Some((&second_installer, &installer_hash)),
            &[],
            0,
        )
        .unwrap();
        let second_contents: serde_json::Value =
            serde_json::from_slice(&fs::read(second_plan).unwrap()).unwrap();
        assert_eq!(contents["lock_path"], second_contents["lock_path"]);
        let locked = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&lock_path)
            .unwrap();
        let mut child = launch(&plan).unwrap();
        let completed = result(&plan, &mut child);
        assert_eq!(completed["code"], "LOCAL_UPDATE_BUSY");
        assert_eq!(completed["phase"], "update-lock");
        drop(locked);
        fs::write(&current, "another native update").unwrap();
        let plan = prepare(
            workspace.path(),
            &current,
            &expected,
            Some((&installer, &installer_hash)),
            &[],
            0,
        )
        .unwrap();
        let mut child = launch(&plan).unwrap();
        let completed = result(&plan, &mut child);
        assert_eq!(completed["code"], "LOCAL_UPDATE_BUSY");
        assert_eq!(completed["phase"], "current-check");
        assert_eq!(
            fs::read_to_string(&current).unwrap(),
            "another native update"
        );
        fs::remove_file(lock_path).unwrap();
    }

    /// 等待已知父进程退出后才更新；仅启动读取标准输入的隔离测试进程。
    /// Applies only after the known parent exits, using an isolated child waiting on standard input.
    #[test]
    fn portable_waits_for_parent_exit() {
        let (workspace, _install, current, binaries) = fixture();
        let system = std::env::var_os("SystemRoot").unwrap();
        let shell = PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut parent = Command::new(shell)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::ReadLine() | Out-Null",
            ])
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let plan = prepare(
            workspace.path(),
            &current,
            &digest(&current).unwrap(),
            None,
            &binaries,
            parent.id(),
        )
        .unwrap();
        let mut child = launch(&plan).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if read_result(&plan).is_some_and(|value| value["phase"] == "wait-parent") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "helper did not reach parent wait"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(parent.try_wait().unwrap().is_none());
        for (_, target) in &binaries {
            assert!(fs::read_to_string(target).unwrap().starts_with("old "));
        }
        drop(parent.stdin.take());
        parent.wait().unwrap();
        assert_eq!(result(&plan, &mut child)["status"], "success");
        for (source, target) in binaries {
            assert_eq!(fs::read(source).unwrap(), fs::read(target).unwrap());
        }
    }
}
