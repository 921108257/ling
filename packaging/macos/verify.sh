#!/usr/bin/env bash
# 仅供 CI 验证原生安装生命周期。
# Validate the native installation lifecycle on CI hosts only.
set -euo pipefail
: "${VERSION:?VERSION is required}"
: "${ARCH:?ARCH is required}"
artifact="dist/ling_${VERSION}_macos_${ARCH}.pkg"
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
