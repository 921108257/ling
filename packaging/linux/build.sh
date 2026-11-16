#!/usr/bin/env bash
# 将已编译的静态二进制打包，不构建或启动服务。
# Package prebuilt static binaries without building or starting the service.
set -euo pipefail

: "${VERSION:?Set VERSION without the v prefix}"
: "${TARGET:?Set TARGET to the Rust target triple}"
: "${ARCH:?Set ARCH to x86_64 or aarch64}"
version=$VERSION
target=$TARGET
arch=$ARCH
case "$target:$arch" in
    x86_64-unknown-linux-musl:x86_64) export NFPM_ARCH=amd64 ;;
    aarch64-unknown-linux-musl:aarch64) export NFPM_ARCH=arm64 ;;
    *) echo "Unsupported target/architecture: $target/$arch" >&2; exit 2 ;;
esac
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "Expected a semantic version without the v prefix: $version" >&2
    exit 2
fi

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"
command -v nfpm >/dev/null
command -v appimagetool >/dev/null
mkdir -p target dist
stage=$(mktemp -d "$repo_root/target/package-linux-${arch}.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
bundle="$stage/ling"
mkdir -p "$bundle/docs"
install -m 0755 "target/$target/release/ling" "$bundle/ling"
install -m 0755 "target/$target/release/ling-cli" "$bundle/ling-cli"
install -m 0644 packaging/ling.toml "$bundle/ling.example.toml"
install -m 0644 packaging/linux/ling.toml "$bundle/ling.service.toml"
install -m 0644 README.md "$bundle/"
install -m 0644 docs/*.md "$bundle/docs/"

export PACKAGE_VERSION="$version"
export NFPM_STAGE="$bundle"
artifact="$repo_root/dist/ling_${version}_linux_${arch}"
nfpm package --config packaging/linux/nfpm.yaml --packager deb --target "$artifact.deb"
nfpm package --config packaging/linux/nfpm.yaml --packager rpm --target "$artifact.rpm"
tar -czf "$artifact.tar.gz" -C "$stage" ling

# AppImage 与普通安装包使用完全相同的二进制与说明文件。
# AppImage shares the exact binaries and documentation with native packages.
app_dir="$stage/Linger.AppDir"
mkdir -p "$app_dir/usr/bin" "$app_dir/usr/share/ling"
install -m 0755 "$bundle/ling" "$bundle/ling-cli" "$app_dir/usr/bin/"
cp -R "$bundle/docs" "$app_dir/usr/share/ling/"
install -m 0644 "$bundle/README.md" "$bundle/ling.example.toml" "$app_dir/usr/share/ling/"
install -m 0755 packaging/linux/AppRun "$app_dir/AppRun"
install -m 0644 packaging/linux/ling.desktop packaging/linux/ling.svg "$app_dir/"
ARCH="$arch" APPIMAGE_EXTRACT_AND_RUN=1 appimagetool "$app_dir" "$artifact.AppImage"
chmod 0755 "$artifact.AppImage"
