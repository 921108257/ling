#!/usr/bin/env bash
# 仅供 CI 验证原生安装生命周期。
# Validate the native installation lifecycle on CI hosts only.
set -euo pipefail
: "${VERSION:?VERSION is required}"
: "${ARCH:?ARCH is required}"
artifact="dist/ling_${VERSION}_macos_${ARCH}.pkg"
# 先检查便携归档的原文；Inspect the portable archive's license texts before installing.
tar -xOf "dist/ling_${VERSION}_macos_${ARCH}.tar.gz" ling/LICENSE | cmp LICENSE -
tar -xOf "dist/ling_${VERSION}_macos_${ARCH}.tar.gz" ling/THIRD-PARTY-NOTICES.txt > target/macos-portable-notices.txt
grep -q '^ring ' target/macos-portable-notices.txt
grep -q '^Rust standard library ' target/macos-portable-notices.txt
wait_for_service() {
    for ((attempt=0; attempt<30; attempt++)); do
        if sudo launchctl print system/io.ling.cache >/dev/null 2>&1 && /usr/local/bin/ling-cli --lang en-US -c PING; then return; fi
        sleep 1
    done
    sudo launchctl print system/io.ling.cache
    return 1
}
sudo installer -pkg "$artifact" -target /
wait_for_service
cmp LICENSE /usr/local/share/ling/LICENSE
test -r /usr/local/share/ling/THIRD-PARTY-NOTICES.txt
grep -q '^ring ' /usr/local/share/ling/THIRD-PARTY-NOTICES.txt
grep -q '^Rust standard library ' /usr/local/share/ling/THIRD-PARTY-NOTICES.txt
sudo sh -c 'printf "\n# preserved during package upgrade\n" >> /usr/local/etc/ling/ling.toml'
config_hash=$(sudo shasum -a 256 /usr/local/etc/ling/ling.toml)
sudo touch /usr/local/var/lib/ling/preserve-me
sudo installer -pkg "$artifact" -target /
wait_for_service
test "$(sudo shasum -a 256 /usr/local/etc/ling/ling.toml)" = "$config_hash"
sudo /usr/local/bin/ling-uninstall
if sudo launchctl print system/io.ling.cache >/dev/null 2>&1; then exit 1; fi
test "$(sudo shasum -a 256 /usr/local/etc/ling/ling.toml)" = "$config_hash"
sudo test -f /usr/local/var/lib/ling/preserve-me
sudo touch /usr/local/etc/ling/no-autostart
sudo installer -pkg "$artifact" -target /
if sudo launchctl print system/io.ling.cache >/dev/null 2>&1; then exit 1; fi
sudo /usr/local/bin/ling-uninstall
