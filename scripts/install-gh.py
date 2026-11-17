#!/usr/bin/env python3
"""仅为 CI 安装经摘要校验的官方 gh；Install digest-pinned official gh for CI only."""

import hashlib
import os
from pathlib import Path
import platform
import shutil
import stat
import tarfile
import tempfile
import urllib.parse
import urllib.request
import zipfile


VERSION = "2.97.0"
MAX_BYTES = 128 * 1024 * 1024
ASSETS = {
    ("Linux", "x86_64"): ("linux_amd64.tar.gz", "a2c9b8497e1f85b1ad0dfcb78b5a622e098801b8e461e459e88e1ee12f018112"),
    ("Linux", "aarch64"): ("linux_arm64.tar.gz", "73ea440ecad9c9e284429997ee6f93577bc6f7bc6fba357ef62c53ad8fb641a5"),
    ("Darwin", "x86_64"): ("macOS_amd64.zip", "63298c998cc2a924c9e254c6af6a1caad6ece281122687a91f079bc0a462700e"),
    ("Darwin", "aarch64"): ("macOS_arm64.zip", "a58b8fd77b417a38f47a0b54d1370c59b0fcdb324ccc9ca002b0998f7c4c999e"),
    ("Windows", "x86_64"): ("windows_amd64.zip", "35d7fe05c4dd1411ffda1e73dfc7c6f44b75c936ca51fa6595c657fdc0350cec"),
}


class HttpsRedirects(urllib.request.HTTPRedirectHandler):
    """重定向也保持 HTTPS；Keep redirects on HTTPS too."""

    def redirect_request(self, request, response, code, message, headers, url):
        if urllib.parse.urlsplit(url).scheme != "https":
            raise ValueError("gh download redirected away from HTTPS")
        return super().redirect_request(request, response, code, message, headers, url)


def platform_asset(system, machine):
    """将 runner 的平台名称映射到明确支持的资产；Map runner platform names to supported assets."""
    machine = machine.lower()
    normalized = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    try:
        suffix, digest = ASSETS[(system, normalized)]
    except KeyError as error:
        raise ValueError(f"Unsupported gh CI platform: {system}/{machine}") from error
    filename = f"gh_{VERSION}_{suffix}"
    binary = "gh.exe" if system == "Windows" else "gh"
    directory = filename.removesuffix(".tar.gz").removesuffix(".zip")
    member = f"bin/{binary}" if system == "Windows" else f"{directory}/bin/{binary}"
    return filename, digest, member, binary


def download(url, destination, expected):
    """有界下载并在读取归档前核对固定摘要；Bound downloads and verify the pin before archive access."""
    digest = hashlib.sha256()
    count = 0
    opener = urllib.request.build_opener(HttpsRedirects())
    request = urllib.request.Request(url, headers={"User-Agent": "ling-ci-pinned-gh"})
    with opener.open(request, timeout=60) as response, destination.open("wb") as output:
        while chunk := response.read(1024 * 1024):
            count += len(chunk)
            if count > MAX_BYTES:
                raise ValueError("gh archive exceeds download limit")
            digest.update(chunk)
            output.write(chunk)
    if digest.hexdigest() != expected:
        raise ValueError("gh archive SHA256 mismatch")


def copy_member(source, destination, size):
    """只写所选可执行文件，大小需与归档声明一致；Write only the chosen executable at its declared size."""
    if not 0 < size <= MAX_BYTES:
        raise ValueError("Invalid gh executable size")
    with destination.open("wb") as output:
        shutil.copyfileobj(source, output)
    if destination.stat().st_size != size:
        raise ValueError("Truncated gh executable")
    destination.chmod(0o755)


def extract_binary(archive, member_name, destination):
    """只读取一个固定普通成员，不解包路径或链接；Read one fixed regular member without extracting paths or links."""
    if archive.name.endswith(".tar.gz"):
        with tarfile.open(archive, "r:gz") as source:
            matches = [member for member in source.getmembers() if member.name == member_name]
            if len(matches) != 1 or not matches[0].isfile():
                raise ValueError("Expected one regular gh archive member")
            member = matches[0]
            with source.extractfile(member) as executable:
                copy_member(executable, destination, member.size)
    else:
        with zipfile.ZipFile(archive) as source:
            matches = [member for member in source.infolist() if member.filename == member_name]
            if len(matches) != 1 or matches[0].is_dir():
                raise ValueError("Expected one regular gh archive member")
            member = matches[0]
            file_type = stat.S_IFMT(member.external_attr >> 16)
            if file_type not in (0, stat.S_IFREG):
                raise ValueError("gh archive member is a link or special file")
            with source.open(member) as executable:
                copy_member(executable, destination, member.file_size)


def install(runner_temp, github_path, system, machine):
    """验证并原子发布 gh，供后续 CI 步骤使用；Verify and atomically publish gh for later CI steps."""
    filename, digest, member, binary = platform_asset(system, machine)
    directory = runner_temp.resolve() / f"ling-gh-{VERSION}" / "bin"
    directory.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ling-gh-download-", dir=runner_temp) as temporary:
        temporary = Path(temporary)
        archive = temporary / filename
        url = f"https://github.com/cli/cli/releases/download/v{VERSION}/{filename}"
        download(url, archive, digest)
        staged = temporary / binary
        extract_binary(archive, member, staged)
        staged.replace(directory / binary)
    with github_path.open("a", encoding="utf-8", newline="\n") as output:
        output.write(str(directory) + "\n")
    print(f"Installed official gh {VERSION}: {directory / binary}")


def main():
    """必须使用 CI 提供的临时目录和 PATH 输出文件；Require CI-owned temporary storage and PATH output."""
    install(Path(os.environ["RUNNER_TEMP"]), Path(os.environ["GITHUB_PATH"]), platform.system(), platform.machine())


if __name__ == "__main__":
    main()
