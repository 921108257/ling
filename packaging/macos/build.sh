#!/usr/bin/env bash
# 构建原生 PKG 与便携包；Build a native PKG and portable archive.
set -euo pipefail
: "${VERSION:?VERSION is required}"
: "${TARGET:?TARGET is required}"
: "${ARCH:?ARCH is required}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
stage="$root/target/package-macos-$ARCH"
portable="$stage/linger-cache-db"
install_root="$stage/root/usr/local"
mkdir -p "$portable/bin" "$portable/docs" "$install_root/bin" "$install_root/share/linger-cache-db/docs" dist
for binary in linger-server linger-cli; do
  install -m 755 "target/$TARGET/release/$binary" "$portable/bin/$binary"
  install -m 755 "target/$TARGET/release/$binary" "$install_root/bin/$binary"
done
cp packaging/linger.toml "$portable/linger.example.toml"
cp README.md "$portable/"
cp docs/config.md docs/persistence.md "$portable/docs/"
cp packaging/linger.toml "$install_root/share/linger-cache-db/linger.example.toml"
cp README.md "$install_root/share/linger-cache-db/"
cp docs/config.md docs/persistence.md "$install_root/share/linger-cache-db/docs/"
pkgbuild --root "$stage/root" --identifier io.linger.cache-db --version "$VERSION" \
  --install-location / "dist/linger-cache-db_${VERSION}_macos_${ARCH}.pkg"
tar -czf "dist/linger-cache-db_${VERSION}_macos_${ARCH}.tar.gz" -C "$stage" linger-cache-db
