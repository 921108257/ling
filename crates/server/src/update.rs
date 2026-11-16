//! 官方发行版更新：固定来源、有界下载、校验及安装方式识别。
//! Official release updates with a fixed source, bounded downloads, verification and install detection.

use clap::Args as ClapArgs;
use ling_cli_help::{Context, Diagnostic, Language};
use reqwest::blocking::Client;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[cfg(windows)]
mod windows;

const API: &str = "https://api.github.com/repos/921108257/ling/releases";
const DOWNLOAD: &str = "https://github.com/921108257/ling/releases/download";
const MAX_PACKAGE: u64 = 256 * 1024 * 1024;
const MAX_METADATA: u64 = 2 * 1024 * 1024;

/// 更新选项；更新在加载服务配置之前执行。
/// Update options; updates run before loading server configuration.
#[derive(ClapArgs)]
pub(super) struct Args {
    /// 仅检查版本，不下载或安装；Check versions without downloading or installing.
    #[arg(long)]
    check: bool,
    /// 指定目标版本，不允许降级；Select a target version without permitting downgrades.
    #[arg(long = "version", value_name = "VERSION")]
    target_version: Option<String>,
    /// 仅选择稳定发行版；Select stable releases only.
    #[arg(long)]
    stable: bool,
}

/// 仅反序列化更新需要的官方发行字段。
/// Deserializes only the official release fields needed for updating.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    prerelease: bool,
    draft: bool,
    assets: Vec<Asset>,
}

/// 资产名称必须与平台、架构及版本精确匹配。
/// Asset names must exactly match the platform, architecture and version.
#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// 安装归属决定使用原生安装器或仅更新便携程序。
/// Installation ownership selects a native installer or a portable binary update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Install {
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "linux")]
    Deb,
    #[cfg(target_os = "linux")]
    Rpm,
    #[cfg(target_os = "macos")]
    Pkg,
    Portable,
    #[cfg(target_os = "linux")]
    AppImage,
}

/// 选择、验证和安装；检查模式不会写入安装目录。
/// Selects, verifies and installs; check mode never writes to the installation directory.
pub(super) fn run(args: Args, language: Language) -> Result<(), Diagnostic> {
    let fail = |code| Diagnostic::new(language, code, Context::Update);
    let current = Version::parse(env!("CARGO_PKG_VERSION")).expect("package version");
    let requested = args
        .target_version
        .as_deref()
        .map(|value| Version::parse(value.strip_prefix('v').unwrap_or(value)))
        .transpose()
        .map_err(|_| fail("LOCAL_UPDATE_VERSION"))?;
    if requested.as_ref().is_some_and(|version| version < &current) {
        return Err(fail("LOCAL_UPDATE_VERSION"));
    }
    let executable = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|_| fail("LOCAL_UPDATE_INSTALL"))?;
    #[cfg(target_os = "linux")]
    let fingerprint = appimage_path(&executable).unwrap_or_else(|| executable.clone());
    #[cfg(not(target_os = "linux"))]
    let fingerprint = executable.clone();
    let original_hash = file_hash(&fingerprint).map_err(|_| fail("LOCAL_UPDATE_INSTALL"))?;
    let mut version_command = Command::new(&fingerprint);
    #[cfg(target_os = "linux")]
    if fingerprint != executable {
        version_command.arg("--appimage-extract-and-run");
    }
    let disk_version = version_command
        .arg("--version")
        .output()
        .map_err(|_| fail("LOCAL_UPDATE_BUSY"))?;
    if !disk_version.status.success()
        || String::from_utf8_lossy(&disk_version.stdout).trim() != format!("ling {current}")
    {
        return Err(fail("LOCAL_UPDATE_BUSY"));
    }
    let client = Client::builder()
        .user_agent(concat!("ling/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|_| fail("LOCAL_UPDATE_NETWORK"))?;
    println!("{}", text(language, "checking"));
    let releases: Vec<Release> = if let Some(version) = &requested {
        let bytes = fetch(&client, &format!("{API}/tags/v{version}"), MAX_METADATA)
            .map_err(|error| fail(network_code(error.as_ref())))?;
        vec![serde_json::from_slice(&bytes).map_err(|_| fail("LOCAL_UPDATE_RELEASE"))?]
    } else {
        let bytes = fetch(&client, &format!("{API}?per_page=100"), MAX_METADATA)
            .map_err(|error| fail(network_code(error.as_ref())))?;
        serde_json::from_slice(&bytes).map_err(|_| fail("LOCAL_UPDATE_RELEASE"))?
    };
    let (release, version) = select_release(&releases, requested.as_ref(), args.stable)
        .ok_or_else(|| fail("LOCAL_UPDATE_RELEASE"))?;
    println!(
        "{} {current}\n{} {version}",
        text(language, "current"),
        text(language, "available")
    );
    if version <= current {
        println!("{}", text(language, "latest"));
        return Ok(());
    }
    if args.check {
        println!("{}", text(language, "check_hint"));
        return Ok(());
    }
    let (install, destination) =
        detect_install(&executable).map_err(|_| fail("LOCAL_UPDATE_PLATFORM"))?;
    #[cfg(unix)]
    let _lock = installation_lock(&destination).map_err(|error| {
        fail(if error.kind() == io::ErrorKind::PermissionDenied {
            "LOCAL_UPDATE_PERMISSION"
        } else {
            "LOCAL_UPDATE_BUSY"
        })
    })?;
    if file_hash(&fingerprint).map_err(|_| fail("LOCAL_UPDATE_INSTALL"))? != original_hash {
        return Err(fail("LOCAL_UPDATE_BUSY"));
    }
    let name = asset_name(&version, install).ok_or_else(|| fail("LOCAL_UPDATE_PLATFORM"))?;
    let asset = release_asset(release, &version, &name, MAX_PACKAGE)
        .ok_or_else(|| fail("LOCAL_UPDATE_RELEASE"))?;
    let sums = release_asset(release, &version, "SHA256SUMS", MAX_METADATA)
        .ok_or_else(|| fail("LOCAL_UPDATE_RELEASE"))?;
    let sums = fetch(&client, &sums.browser_download_url, MAX_METADATA)
        .map_err(|_| fail("LOCAL_UPDATE_NETWORK"))?;
    let expected = checksum(&sums, &name).ok_or_else(|| fail("LOCAL_UPDATE_CHECKSUM"))?;
    let workspace = tempfile::Builder::new()
        .prefix("ling-update-")
        .tempdir()
        .map_err(|_| fail("LOCAL_UPDATE_INSTALL"))?;
    let package = workspace.path().join(&name);
    println!("{} {version}", text(language, "downloading"));
    download(&client, asset, &package, &expected).map_err(&fail)?;
    println!("{}", text(language, "verified"));
    if file_hash(&fingerprint).map_err(|_| fail("LOCAL_UPDATE_INSTALL"))? != original_hash {
        return Err(fail("LOCAL_UPDATE_BUSY"));
    }
    let binaries = if install == Install::Portable {
        extract_binaries(&package, workspace.path(), &destination, &version)
            .map_err(|_| fail("LOCAL_UPDATE_INSTALL"))?
    } else {
        Vec::new()
    };
    #[cfg(windows)]
    {
        let installer =
            (install == Install::Windows).then_some((package.as_path(), expected.as_str()));
        let result = windows::schedule(
            workspace.path(),
            &executable,
            &original_hash,
            installer,
            &binaries,
        )
        .map_err(|_| fail("LOCAL_UPDATE_INSTALL"))?;
        // 子进程必须等待当前映像退出；保留暂存文件直至应用完成。
        // The worker waits for this executable to exit; retain staging until application completes.
        let _ = workspace.keep();
        println!(
            "{}\n{} {}",
            text(language, "scheduled"),
            text(language, "result"),
            result.display()
        );
    }
    #[cfg(unix)]
    {
        println!("{}", text(language, "installing"));
        match install {
            #[cfg(target_os = "linux")]
            Install::Deb => install_with("dpkg", &["--force-confold", "-i"], &package),
            #[cfg(target_os = "linux")]
            Install::Rpm => install_with("rpm", &["-Uvh"], &package),
            #[cfg(target_os = "macos")]
            Install::Pkg => install_with("installer", &["-pkg"], &package),
            #[cfg(target_os = "linux")]
            Install::AppImage => replace_binaries(&[(package, destination)]),
            Install::Portable => replace_binaries(&binaries),
        }
        .map_err(|_| fail("LOCAL_UPDATE_INSTALL"))?;
        println!("{} {version}", text(language, "complete"));
    }
    #[cfg(not(any(unix, windows)))]
    return Err(fail("LOCAL_UPDATE_PLATFORM"));
    Ok(())
}

/// 比较语义版本而非发布时间，并明确过滤草稿和稳定通道。
/// Compares semantic versions rather than publish time and explicitly filters drafts and stable channels.
fn select_release<'a>(
    releases: &'a [Release],
    requested: Option<&Version>,
    stable: bool,
) -> Option<(&'a Release, Version)> {
    releases
        .iter()
        .filter_map(|release| {
            if release.draft || (stable && release.prerelease) {
                return None;
            }
            let version = Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
            if (stable && !version.pre.is_empty())
                || requested.is_some_and(|target| target != &version)
            {
                return None;
            }
            Some((release, version))
        })
        .max_by(|(_, a), (_, b)| a.cmp(b))
}

/// 资产仅允许来自固定仓库的精确下载路径，重名资产拒绝使用。
/// Allows only exact download paths in the fixed repository and rejects duplicate asset names.
fn release_asset<'a>(
    release: &'a Release,
    version: &Version,
    name: &str,
    limit: u64,
) -> Option<&'a Asset> {
    let mut matches = release.assets.iter().filter(|asset| asset.name == name);
    let asset = matches.next()?;
    (matches.next().is_none()
        && asset.size > 0
        && asset.size <= limit
        && asset.browser_download_url == format!("{DOWNLOAD}/v{version}/{name}"))
    .then_some(asset)
}

/// 下载控制面数据并限制实际响应长度，不信任 Content-Length。
/// Downloads control data with an actual body bound rather than trusting Content-Length.
fn fetch(client: &Client, url: &str, limit: u64) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut request = client.get(url);
    // 可选令牌仅发送给固定 API 源，不附加到下载 CDN 或其他请求。
    // Optional tokens go only to the fixed API origin, never download CDNs or other requests.
    if url.starts_with(API) {
        if let Ok(token) = std::env::var("LING_GITHUB_TOKEN") {
            if !token.trim().is_empty() {
                request = request.bearer_auth(token);
            }
        }
    }
    let response = request.send()?.error_for_status()?;
    let mut bytes = Vec::new();
    // 实际超限与网络截断均不接受部分 JSON；调用方统一输出网络诊断。
    // Oversized or truncated bodies never become partial JSON; callers use a network diagnostic.
    response.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("response body limit").into());
    }
    Ok(bytes)
}

/// 将 GitHub 限流与普通连接故障区分，保持秘密不进入诊断。
/// Distinguishes GitHub rate limits from transport failures without exposing secrets.
fn network_code(error: &(dyn std::error::Error + 'static)) -> &'static str {
    if error
        .downcast_ref::<reqwest::Error>()
        .and_then(|error| error.status())
        .is_some_and(|status| status.as_u16() == 403 || status.as_u16() == 429)
    {
        "LOCAL_UPDATE_RATE_LIMIT"
    } else {
        "LOCAL_UPDATE_NETWORK"
    }
}

/// 严格解析两空格分隔的校验清单，仅接受唯一的精确文件名。
/// Strictly parses two-space checksum lines and accepts one exact filename only.
fn checksum(bytes: &[u8], name: &str) -> Option<String> {
    let mut result = None;
    for line in std::str::from_utf8(bytes).ok()?.lines() {
        let (digest, file) = line.split_once("  ")?;
        if digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || file.contains(['/', '\\'])
            || file.is_empty()
        {
            return None;
        }
        if file == name {
            if result.is_some() {
                return None;
            }
            result = Some(digest.to_ascii_lowercase());
        }
    }
    result
}

/// 流式校验文件，失败时不会触碰已安装的程序。
/// Streams and verifies the package without touching installed programs on failure.
fn download(
    client: &Client,
    asset: &Asset,
    destination: &Path,
    expected: &str,
) -> Result<(), &'static str> {
    let mut response = client
        .get(&asset.browser_download_url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|_| "LOCAL_UPDATE_NETWORK")?;
    let mut file = File::create(destination).map_err(|_| "LOCAL_UPDATE_INSTALL")?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = response
            .read(&mut buffer)
            .map_err(|_| "LOCAL_UPDATE_NETWORK")?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > asset.size || total > MAX_PACKAGE {
            return Err("LOCAL_UPDATE_CHECKSUM");
        }
        digest.update(&buffer[..count]);
        file.write_all(&buffer[..count])
            .map_err(|_| "LOCAL_UPDATE_INSTALL")?;
    }
    if total != asset.size || format!("{:x}", digest.finalize()) != expected {
        return Err("LOCAL_UPDATE_CHECKSUM");
    }
    file.sync_all().map_err(|_| "LOCAL_UPDATE_INSTALL")
}

/// 根据当前安装归属选择包，不将系统包文件当作普通便携文件覆盖。
/// Selects by installation ownership rather than overwriting package-owned files as portable binaries.
fn detect_install(executable: &Path) -> io::Result<(Install, PathBuf)> {
    let unsupported = || io::Error::other("unsupported installation");
    #[cfg(windows)]
    {
        let directory = executable.parent().ok_or_else(unsupported)?;
        if directory
            .parent()
            .is_some_and(|root| root.join("unins000.exe").is_file())
        {
            return Ok((Install::Windows, executable.to_owned()));
        }
        if directory.join("ling-cli.exe").is_file() {
            return Ok((Install::Portable, executable.to_owned()));
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(image) = appimage_path(executable) {
            return Ok((Install::AppImage, image));
        }
        if executable == Path::new("/usr/bin/ling") {
            if output_is(
                "dpkg-query",
                &["-S", "/usr/bin/ling"],
                "ling: /usr/bin/ling",
            ) {
                return Ok((Install::Deb, executable.to_owned()));
            }
            if output_is(
                "rpm",
                &["-qf", "--queryformat", "%{NAME}", "/usr/bin/ling"],
                "ling",
            ) {
                return Ok((Install::Rpm, executable.to_owned()));
            }
            return Err(unsupported());
        }
    }
    #[cfg(target_os = "macos")]
    if executable == Path::new("/usr/local/bin/ling") {
        if Command::new("pkgutil")
            .args(["--pkg-info", "io.ling.pkg"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return Ok((Install::Pkg, executable.to_owned()));
        }
        return Err(unsupported());
    }
    #[cfg(unix)]
    if executable
        .parent()
        .is_some_and(|path| path.join("ling-cli").is_file())
        && !executable.starts_with("/usr/bin")
        && !executable.starts_with("/bin")
    {
        return Ok((Install::Portable, executable.to_owned()));
    }
    Err(unsupported())
}

/// 验证包数据库所有权，不向用户显示外部程序输出。
/// Verifies package-database ownership without displaying external output.
#[cfg(target_os = "linux")]
fn output_is(program: &str, args: &[&str], expected: &str) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == expected
        })
}

/// AppImage 的更新目标是外层文件，不是只读挂载中的映像。
/// AppImage updates target the outer file rather than the image in its read-only mount.
#[cfg(target_os = "linux")]
fn appimage_path(executable: &Path) -> Option<PathBuf> {
    let appdir = fs::canonicalize(std::env::var_os("APPDIR")?).ok()?;
    let image = PathBuf::from(std::env::var_os("APPIMAGE")?);
    (executable.starts_with(appdir) && image.is_absolute() && image.is_file())
        .then(|| fs::canonicalize(image).ok())
        .flatten()
}

/// 根据编译目标和安装类型生成官方资产名。
/// Builds the official asset name from the compilation target and installation type.
fn asset_name(version: &Version, install: Install) -> Option<String> {
    let arch = std::env::consts::ARCH;
    if !matches!(arch, "x86_64" | "aarch64") {
        return None;
    }
    let (platform, extension) = match install {
        #[cfg(windows)]
        Install::Windows if arch == "x86_64" => ("windows", "exe"),
        #[cfg(target_os = "linux")]
        Install::Deb => ("linux", "deb"),
        #[cfg(target_os = "linux")]
        Install::Rpm => ("linux", "rpm"),
        #[cfg(target_os = "macos")]
        Install::Pkg => ("macos", "pkg"),
        #[cfg(target_os = "linux")]
        Install::AppImage => ("linux", "AppImage"),
        Install::Portable if cfg!(windows) && arch == "x86_64" => ("windows", "zip"),
        Install::Portable if cfg!(target_os = "linux") => ("linux", "tar.gz"),
        Install::Portable if cfg!(target_os = "macos") => ("macos", "tar.gz"),
        _ => return None,
    };
    #[cfg(windows)]
    let suffix = if install == Install::Windows {
        "_setup"
    } else {
        ""
    };
    #[cfg(not(windows))]
    let suffix = "";
    Some(format!(
        "ling_{version}_{platform}_{arch}{suffix}.{extension}"
    ))
}

/// 只提取两个固定成员，不解包配置、链接或任意路径。
/// Extracts only two fixed members, never configuration, links or arbitrary paths.
fn extract_binaries(
    package: &Path,
    workspace: &Path,
    executable: &Path,
    version: &Version,
) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let names = if cfg!(windows) {
        ["ling.exe", "ling-cli.exe"]
    } else {
        ["ling", "ling-cli"]
    };
    let archive_prefix = if cfg!(target_os = "linux") {
        "ling/"
    } else {
        "ling/bin/"
    };
    let mut found = [false; 2];
    if cfg!(windows) {
        let mut archive = zip::ZipArchive::new(File::open(package)?)?;
        if archive.len() > 1024 {
            return Err(io::Error::other("archive member limit"));
        }
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            if let Some(index) = names
                .iter()
                .position(|name| entry.name() == format!("{archive_prefix}{name}"))
            {
                if entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
                {
                    return Err(io::Error::other("archive link"));
                }
                extract_member(&mut entry, &workspace.join(names[index]), &mut found[index])?;
            }
        }
    } else {
        let decoded = flate2::read::GzDecoder::new(File::open(package)?).take(MAX_PACKAGE * 2);
        let mut archive = tar::Archive::new(decoded);
        for (count, entry) in archive.entries()?.enumerate() {
            if count >= 1024 {
                return Err(io::Error::other("archive member limit"));
            }
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            if let Some(index) = names
                .iter()
                .position(|name| path == Path::new(&format!("{archive_prefix}{name}")))
            {
                if !entry.header().entry_type().is_file() {
                    return Err(io::Error::other("archive link"));
                }
                extract_member(&mut entry, &workspace.join(names[index]), &mut found[index])?;
            }
        }
    }
    if found != [true; 2] {
        return Err(io::Error::other("missing release binary"));
    }
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("missing binary directory"))?;
    let mut binaries = Vec::new();
    for name in names {
        let source = workspace.join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;
        }
        let output = Command::new(&source).arg("--version").output()?;
        let expected = format!("{} {version}", name.trim_end_matches(".exe"));
        if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != expected {
            return Err(io::Error::other("release binary version mismatch"));
        }
        binaries.push((source, directory.join(name)));
    }
    Ok(binaries)
}

/// 每个成员有独立大小上限，拒绝重复成员后才创建文件。
/// Bounds each member and rejects duplicates before creating its file.
fn extract_member(reader: &mut impl Read, path: &Path, found: &mut bool) -> io::Result<()> {
    if *found {
        return Err(io::Error::other("duplicate release binary"));
    }
    *found = true;
    let mut file = File::create(path)?;
    if io::copy(&mut reader.take(MAX_PACKAGE + 1), &mut file)? > MAX_PACKAGE {
        return Err(io::Error::other("expanded member limit"));
    }
    file.sync_all()
}

/// 在下载前验证目标目录权限，系统安装要求调用者显式使用 sudo。
/// Checks destination permissions before download; system installs require explicit sudo by the caller.
#[cfg(unix)]
fn installation_lock(destination: &Path) -> io::Result<File> {
    use fs2::FileExt;
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("missing directory"))?;
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".ling-update.lock"))?;
    file.try_lock_exclusive()?;
    Ok(file)
}

/// 原生安装器拥有配置和服务生命周期，失败保持非零结果。
/// Native installers own configuration and service lifecycle; failures remain nonzero.
#[cfg(unix)]
fn install_with(program: &str, args: &[&str], package: &Path) -> io::Result<()> {
    let mut command = Command::new(program);
    command.args(args).arg(package);
    if program == "installer" {
        command.args(["-target", "/"]);
    }
    if command.status()?.success() {
        Ok(())
    } else {
        Err(io::Error::other("installer failed"))
    }
}

/// 在目标文件系统暂存并备份，任一步失败恢复先前的两个程序。
/// Stages on the destination filesystem and restores both prior binaries on any failed step.
#[cfg(unix)]
fn replace_binaries(binaries: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let directory = binaries[0]
        .1
        .parent()
        .ok_or_else(|| io::Error::other("missing directory"))?;
    let stage = tempfile::Builder::new()
        .prefix(".ling-update-")
        .tempdir_in(directory)?;
    for (index, (source, _)) in binaries.iter().enumerate() {
        let target = stage.path().join(format!("new-{index}"));
        fs::copy(source, &target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
        File::open(&target)?.sync_all()?;
    }
    let mut backed_up = Vec::new();
    let result = (|| {
        for (index, (_, destination)) in binaries.iter().enumerate() {
            let backup = stage.path().join(format!("old-{index}"));
            fs::rename(destination, &backup)?;
            backed_up.push((backup, destination));
            fs::rename(stage.path().join(format!("new-{index}")), destination)?;
        }
        File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        for (backup, destination) in backed_up.into_iter().rev() {
            // rename 覆盖已应用的新文件；回滚失败时保留备份目录供恢复。
            // rename replaces an applied new file; preserve backups if rollback itself fails.
            if fs::rename(backup, destination).is_err() {
                let _ = stage.keep();
                return Err(io::Error::other("rollback requires recovery"));
            }
        }
    }
    result
}

/// 记录磁盘映像以检测下载期间的并发升级，避免旧进程覆盖新安装。
/// Fingerprints the disk image to detect concurrent upgrades during download and prevent stale overwrites.
fn file_hash(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// 更新输出仅包含受信任文案、解析后的版本和本机结果文件路径。
/// Update output contains trusted text, parsed versions and the local result-file path only.
fn text(language: Language, key: &str) -> &'static str {
    include_str!("update/messages.tsv")
        .lines()
        .find_map(|line| {
            let mut columns = line.split('\t');
            (columns.next()? == key)
                .then(|| columns.nth(language as usize))
                .flatten()
        })
        .expect("complete update messages")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    /// 构造本地发行目录，不依赖 GitHub 可用性。
    /// Builds local release metadata without depending on GitHub availability.
    fn release(version: &str, prerelease: bool, draft: bool) -> Release {
        Release {
            tag_name: format!("v{version}"),
            prerelease,
            draft,
            assets: Vec::new(),
        }
    }

    /// 预发布默认可选，稳定过滤、显式目标与语义排序独立于发布时间。
    /// Previews are included by default; stable filtering and pinning use semantic rather than publish order.
    #[test]
    fn release_channels_and_pins_use_semantic_order() {
        let releases = [
            release("0.1.9", false, false),
            release("0.1.10", true, false),
            release("9.0.0", false, true),
        ];
        assert_eq!(
            select_release(&releases, None, false)
                .unwrap()
                .1
                .to_string(),
            "0.1.10"
        );
        assert_eq!(
            select_release(&releases, None, true).unwrap().1.to_string(),
            "0.1.9"
        );
        let pinned = Version::parse("0.1.10").unwrap();
        assert!(select_release(&releases, Some(&pinned), true).is_none());
        assert_eq!(
            select_release(&releases, Some(&pinned), false).unwrap().1,
            pinned
        );
    }

    /// 拒绝重名校验行、路径名和非官方资产来源。
    /// Rejects duplicate checksum entries, path names and asset URLs outside the official release.
    #[test]
    fn checksums_and_asset_origin_are_strict() {
        let digest = "a".repeat(64);
        let line = format!("{digest}  package.zip\n");
        assert_eq!(
            checksum(line.as_bytes(), "package.zip"),
            Some(digest.clone())
        );
        assert!(checksum(format!("{line}{line}").as_bytes(), "package.zip").is_none());
        assert!(
            checksum(
                format!("{digest}  ../package.zip\n").as_bytes(),
                "package.zip"
            )
            .is_none()
        );
        assert!(checksum(b"bad  package.zip\n", "package.zip").is_none());
        let version = Version::parse("0.1.3").unwrap();
        let mut release = release("0.1.3", true, false);
        release.assets.push(Asset {
            name: "package.zip".into(),
            browser_download_url: format!("{DOWNLOAD}/v{version}/package.zip"),
            size: 4,
        });
        assert!(release_asset(&release, &version, "package.zip", 5).is_some());
        assert!(release_asset(&release, &version, "package.zip", 3).is_none());
        release.assets[0].browser_download_url = "https://example.com/package.zip".into();
        assert!(release_asset(&release, &version, "package.zip", 5).is_none());
    }

    /// 本地 HTTP 仅用于单元测试；生产 URL 始终来自固定来源校验。
    /// Local HTTP exists only in unit tests; production URLs always pass fixed-origin validation.
    fn response(body: &[u8]) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_vec();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });
        (format!("http://{address}/package"), handle)
    }

    /// 真实下载路径验证字节数与摘要；校验失败不修改旁边的安装文件。
    /// The real download path verifies size and digest while leaving installed files untouched on failure.
    #[test]
    fn download_rejects_corruption_and_oversize() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("installed");
        fs::write(&installed, "original").unwrap();
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        for (size, expected, success) in [
            (4, format!("{:x}", Sha256::digest(b"data")), true),
            (4, "a".repeat(64), false),
            (3, "a".repeat(64), false),
        ] {
            let (url, worker) = response(b"data");
            let asset = Asset {
                name: "package".into(),
                browser_download_url: url,
                size,
            };
            let result = download(&client, &asset, &directory.path().join("staged"), &expected);
            assert_eq!(result.is_ok(), success);
            worker.join().unwrap();
            assert_eq!(fs::read_to_string(&installed).unwrap(), "original");
        }
        let (url, worker) = response(b"too long");
        assert!(fetch(&client, &url, 3).is_err());
        worker.join().unwrap();
    }

    /// 重复或空的二进制成员被拒绝，解包不会覆盖安装目录。
    /// Duplicate or absent binary members are rejected without extracting over the installed directory.
    #[test]
    fn extraction_rejects_duplicate_and_missing_members() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("package");
        if cfg!(windows) {
            let mut archive = zip::ZipWriter::new(File::create(&file).unwrap());
            archive
                .start_file(
                    "ling/bin/ling.exe",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            archive.write_all(b"not executable").unwrap();
            archive.finish().unwrap();
        } else {
            let compressed = flate2::write::GzEncoder::new(
                File::create(&file).unwrap(),
                flate2::Compression::default(),
            );
            let mut archive = tar::Builder::new(compressed);
            let path = if cfg!(target_os = "linux") {
                "ling/ling"
            } else {
                "ling/bin/ling"
            };
            for _ in 0..2 {
                let mut header = tar::Header::new_gnu();
                header.set_size(3);
                header.set_mode(0o755);
                header.set_cksum();
                archive.append_data(&mut header, path, &b"bad"[..]).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap();
        }
        let installed = directory.path().join("original-ling");
        fs::write(&installed, "original").unwrap();
        assert!(
            extract_binaries(
                &file,
                directory.path(),
                &installed,
                &Version::parse("0.1.3").unwrap()
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(installed).unwrap(), "original");
        let mut found = false;
        extract_member(
            &mut &b"first"[..],
            &directory.path().join("member"),
            &mut found,
        )
        .unwrap();
        assert!(
            extract_member(
                &mut &b"second"[..],
                &directory.path().join("member"),
                &mut found
            )
            .is_err()
        );
    }

    /// 便携更新成功后两程序一致；第二文件失败则恢复第一文件和用户配置。
    /// Portable updates keep both programs aligned and restore the first on second-file failure.
    #[cfg(unix)]
    #[test]
    fn portable_replacement_rolls_back_and_preserves_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("ling");
        let second = directory.path().join("ling-cli");
        let source = directory.path().join("new");
        let config = directory.path().join("ling.toml");
        fs::write(&first, "old-server").unwrap();
        fs::write(&source, "new-binary").unwrap();
        fs::write(&config, "keep configuration").unwrap();
        assert!(
            replace_binaries(&[
                (source.clone(), first.clone()),
                (source.clone(), second.clone())
            ])
            .is_err()
        );
        assert_eq!(fs::read_to_string(&first).unwrap(), "old-server");
        fs::write(&second, "old-client").unwrap();
        replace_binaries(&[(source.clone(), first.clone()), (source, second.clone())]).unwrap();
        assert_eq!(fs::read_to_string(first).unwrap(), "new-binary");
        assert_eq!(fs::read_to_string(second).unwrap(), "new-binary");
        assert_eq!(fs::read_to_string(config).unwrap(), "keep configuration");
    }

    /// 所有进度消息都有七语译文且不含开发说明。
    /// Every progress message has seven nonempty translations.
    #[test]
    fn update_messages_cover_all_languages() {
        for line in include_str!("update/messages.tsv").lines() {
            let cells: Vec<_> = line.split('\t').collect();
            assert_eq!(cells.len(), 8);
            assert!(cells.iter().all(|cell| !cell.trim().is_empty()));
        }
        assert!(
            asset_name(&Version::parse("0.1.3").unwrap(), Install::Portable)
                .unwrap()
                .contains("0.1.3")
        );
    }
}
