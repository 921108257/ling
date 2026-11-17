#!/usr/bin/env bash
# 构建原生 PKG 与便携包；Build a native PKG and portable archive.
set -euo pipefail
: "${VERSION:?VERSION is required}"
: "${TARGET:?TARGET is required}"
: "${ARCH:?ARCH is required}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
mkdir -p target dist
stage=$(mktemp -d "$root/target/package-macos-${ARCH}.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
portable="$stage/ling"
install_root="$stage/root/usr/local"
mkdir -p "$portable/bin" "$portable/docs" "$install_root/bin" "$install_root/share/ling/docs" "$stage/root/Library/LaunchDaemons" "$stage/scripts"
for binary in ling ling-cli; do
  install -m 755 "target/$TARGET/release/$binary" "$portable/bin/$binary"
  install -m 755 "target/$TARGET/release/$binary" "$install_root/bin/$binary"
done
cp packaging/ling.toml "$portable/ling.example.toml"
cp README.md "$portable/"
cp LICENSE "$portable/"
python3 packaging/generate-notices.py --target "$TARGET" --output "$portable/THIRD-PARTY-NOTICES.txt"
cp docs/*.md "$portable/docs/"
cp packaging/ling.toml "$install_root/share/ling/ling.example.toml"
cp packaging/macos/ling.toml "$install_root/share/ling/ling.service.toml"
cp README.md "$install_root/share/ling/"
cp "$portable/LICENSE" "$portable/THIRD-PARTY-NOTICES.txt" "$install_root/share/ling/"
cp docs/*.md "$install_root/share/ling/docs/"
install -m 0644 packaging/macos/io.ling.cache.plist "$stage/root/Library/LaunchDaemons/"
install -m 0755 packaging/macos/uninstall.sh "$install_root/bin/ling-uninstall"
install -m 0755 packaging/macos/preinstall packaging/macos/postinstall "$stage/scripts/"
# 卸载清单在打包时固定，只移除当前包拥有的文档。
# Freeze the uninstall manifest at build time; remove only package-owned documents.
for file in docs/*.md; do
  printf '/usr/local/share/ling/%s\n' "$file" >> "$install_root/share/ling/installed-docs.txt"
done
pkgbuild --root "$stage/root" --identifier io.ling.pkg --version "$VERSION" --scripts "$stage/scripts" \
  --install-location / "dist/ling_${VERSION}_macos_${ARCH}.pkg"
tar -czf "dist/ling_${VERSION}_macos_${ARCH}.tar.gz" -C "$stage" ling
