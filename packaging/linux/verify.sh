#!/usr/bin/env bash
# 在 CI 原生主机验证安装、自动启动、升级保留和卸载；不用于用户机器。
# Verify install, automatic start, upgrade preservation and removal on native CI hosts only.
set -euo pipefail
: "${VERSION:?VERSION is required}"
: "${ARCH:?ARCH is required}"
# 便携包与 AppImage 同样必须携带许可正文，不只验证安装后的副本。
# Portable archives and AppImages must carry readable texts as well as installed packages.
tar -xOf "dist/ling_${VERSION}_linux_${ARCH}.tar.gz" ling/LICENSE | cmp LICENSE -
tar -xOf "dist/ling_${VERSION}_linux_${ARCH}.tar.gz" ling/THIRD-PARTY-NOTICES.txt > target/linux-portable-notices.txt
grep -q '^ring ' target/linux-portable-notices.txt
grep -q '^musl C library ' target/linux-portable-notices.txt
image_dir=$(mktemp -d "$PWD/target/license-appimage.XXXXXX")
trap 'rm -rf -- "$image_dir"' EXIT
image="$PWD/dist/ling_${VERSION}_linux_${ARCH}.AppImage"
(cd "$image_dir" && "$image" --appimage-extract >/dev/null)
cmp LICENSE "$image_dir/squashfs-root/usr/share/ling/LICENSE"
test -r "$image_dir/squashfs-root/usr/share/ling/THIRD-PARTY-NOTICES.txt"
grep -q '^ring ' "$image_dir/squashfs-root/usr/share/ling/THIRD-PARTY-NOTICES.txt"
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
    cmp LICENSE /usr/share/ling/LICENSE
    test -r /usr/share/ling/THIRD-PARTY-NOTICES.txt
    grep -q '^ring ' /usr/share/ling/THIRD-PARTY-NOTICES.txt
    grep -q '^Rust standard library ' /usr/share/ling/THIRD-PARTY-NOTICES.txt
    grep -q '^musl C library ' /usr/share/ling/THIRD-PARTY-NOTICES.txt
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
