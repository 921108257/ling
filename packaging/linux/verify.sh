#!/usr/bin/env bash
# 在 CI 原生主机验证安装、自动启动、升级保留和卸载；不用于用户机器。
# Verify install, automatic start, upgrade preservation and removal on native CI hosts only.
set -euo pipefail
: "${VERSION:?VERSION is required}"
: "${ARCH:?ARCH is required}"
wait_for_service() {
    for ((attempt=0; attempt<30; attempt++)); do
        if systemctl is-active --quiet ling.service && /usr/bin/ling-cli --lang en-US -c PING; then
            return
        fi
        sleep 1
    done
    sudo journalctl -u ling.service --no-pager -n 50
    return 1
}
for package in deb rpm; do
    artifact="dist/ling_${VERSION}_linux_${ARCH}.$package"
    if [ "$package" = deb ]; then
        sudo dpkg -i "$artifact"
    else
        sudo rpm -i --nodeps "$artifact"
    fi
    wait_for_service
    sudo sh -c 'printf "\n# preserved during package upgrade\n" >> /etc/ling/ling.toml'
    grep -q '^LimitNOFILE=' /lib/systemd/system/ling.service
    config_hash=$(sudo sha256sum /etc/ling/ling.toml)
    sudo touch /var/lib/ling/preserve-me
    if [ "$package" = deb ]; then
        sudo dpkg -i "$artifact"
    else
        sudo rpm -U --replacepkgs --nodeps "$artifact"
    fi
    wait_for_service
    test "$(sudo sha256sum /etc/ling/ling.toml)" = "$config_hash"
    sudo test -f /var/lib/ling/preserve-me
    if [ "$package" = deb ]; then
        sudo dpkg --purge ling
    else
        sudo rpm -e ling
    fi
    if systemctl is-active --quiet ling.service; then exit 1; fi
    test "$(sudo sha256sum /etc/ling/ling.toml)" = "$config_hash"
    sudo test -f /var/lib/ling/preserve-me
done
# 显式跳过自动启动仍可完整安装和卸载。
# Explicitly skipping automatic startup still permits installation and removal.
sudo env LING_SKIP_SERVICE=1 dpkg -i "dist/ling_${VERSION}_linux_${ARCH}.deb"
if systemctl is-active --quiet ling.service; then exit 1; fi
sudo dpkg --purge ling
