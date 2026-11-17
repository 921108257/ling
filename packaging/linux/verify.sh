#!/usr/bin/env bash
# 仅在一次性 CI / 虚拟机验收，日志保留环境、摘要、命令与结果。
# Run only on disposable CI hosts / VMs; retain environment, hashes, commands and results.
set -euo pipefail
: "${VERSION:?VERSION is required}"
: "${ARCH:?ARCH is required}"
if [ "${CI:-}" != true ] && [ "${LING_NATIVE_ACCEPTANCE:-}" != 1 ]; then
    echo 'Use a disposable host and set LING_NATIVE_ACCEPTANCE=1 explicitly.' >&2
    exit 1
fi
[ -d /run/systemd/system ] || { echo 'A running systemd host is required; a container is insufficient.' >&2; exit 1; }
[ ! -e /usr/bin/ling ] || { echo 'A clean host without an existing ling installation is required.' >&2; exit 1; }
mkdir -p target/native-acceptance
exec > >(tee "target/native-acceptance/linux-${ARCH}.log") 2>&1
image_dir=''
finish() {
    result=$?
    set +x
    [ -z "$image_dir" ] || rm -rf -- "$image_dir"
    printf 'native_acceptance_exit_code=%s\n' "$result"
}
trap finish EXIT
set -x
date -u '+%Y-%m-%dT%H:%M:%SZ'
uname -a
cat /etc/os-release
systemctl --version
printf 'candidate=%s previous=%s update_target=%s\n' "$VERSION" "${PREVIOUS_VERSION:-not-executed}" "${UPDATE_VERSION:-not-executed}"
# Ubuntu 的 rpm --nodeps 只算兼容演练；原生 RPM 主机不跳过依赖。
# rpm --nodeps on Ubuntu is only a compatibility exercise; native RPM hosts check dependencies.
rpm_options=()
if command -v dpkg >/dev/null 2>&1; then
    rpm_options=(--nodeps)
    echo 'RPM native distribution acceptance: 未执行 / not executed (Debian-family host).'
else
    echo 'RPM native distribution acceptance: executing on the OS recorded above.'
fi
tar -xOf "dist/ling_${VERSION}_linux_${ARCH}.tar.gz" ling/LICENSE | cmp LICENSE -
tar -xOf "dist/ling_${VERSION}_linux_${ARCH}.tar.gz" ling/THIRD-PARTY-NOTICES.txt > target/linux-portable-notices.txt
grep -q '^ring ' target/linux-portable-notices.txt
grep -q '^musl C library ' target/linux-portable-notices.txt
image_dir=$(mktemp -d "$PWD/target/license-appimage.XXXXXX")
image="$PWD/dist/ling_${VERSION}_linux_${ARCH}.AppImage"
sha256sum "$image" "dist/ling_${VERSION}_linux_${ARCH}.tar.gz"
(cd "$image_dir" && "$image" --appimage-extract >/dev/null)
cmp LICENSE "$image_dir/squashfs-root/usr/share/ling/LICENSE"
grep -q '^ring ' "$image_dir/squashfs-root/usr/share/ling/THIRD-PARTY-NOTICES.txt"
wait_for_service() {
    for ((attempt=0; attempt<30; attempt++)); do
        if systemctl is-active --quiet ling.service && /usr/bin/ling-cli --lang en-US -c PING; then return; fi
        sleep 1
    done
    sudo journalctl -u ling.service --no-pager -n 50
    return 1
}
assert_versions() {
    test "$(/usr/bin/ling --version)" = "ling $1"
    test "$(/usr/bin/ling-cli --version)" = "ling-cli $1"
}
install_package() {
    sha256sum "$1"
    if [ "$package" = deb ]; then
        sudo dpkg -i "$1"
    else
        sudo rpm -U --replacepkgs "${rpm_options[@]}" "$1"
    fi
    wait_for_service
    assert_versions "$2"
}
assert_preserved() {
    test "$(sudo sha256sum /etc/ling/ling.toml)" = "$config_hash"
    sudo test -f /var/lib/ling/preserve-me
    # 没有写操作时重启不得重写 Journal；缓存值本身不作恢复承诺。
    # With no new writes, restart must retain journal bytes; cached values are not promised to recover.
    test "$(sudo sha256sum /var/lib/ling/journal/journal.lgj)" = "$journal_hash"
}
remove_package() {
    if [ "$package" = deb ]; then sudo dpkg --purge ling; else sudo rpm -e ling; fi
    if systemctl is-active --quiet ling.service; then return 1; fi
    test "$(systemctl show ling.service --property=LoadState --value)" = not-found
    assert_preserved
}
for package in ${PACKAGES:-deb rpm}; do
    case "$package" in deb|rpm) ;; *) echo "Unsupported package: $package" >&2; exit 1 ;; esac
    artifact="dist/ling_${VERSION}_linux_${ARCH}.$package"
    install_package "$artifact" "$VERSION"
    sudo systemctl stop ling.service
    if systemctl is-active --quiet ling.service; then exit 1; fi
    sudo systemctl start ling.service
    wait_for_service
    sudo systemctl restart ling.service
    wait_for_service
    cmp LICENSE /usr/share/ling/LICENSE
    grep -q '^ring ' /usr/share/ling/THIRD-PARTY-NOTICES.txt
    grep -q '^Rust standard library ' /usr/share/ling/THIRD-PARTY-NOTICES.txt
    grep -q '^musl C library ' /usr/share/ling/THIRD-PARTY-NOTICES.txt
    grep -q '^LimitNOFILE=' /lib/systemd/system/ling.service
    # 只启用打包配置中已提供的写目录，不修改 systemd 的写入沙箱。
    # Enable only the packaged writable journal path; do not weaken the systemd sandbox.
    sudo sed -i -e 's/^mode = "fast"$/mode = "reliable"/' -e 's/^# \[event_journal\]$/[event_journal]/' -e 's|^# dir = "/var/lib/ling/journal"$|dir = "/var/lib/ling/journal"|' /etc/ling/ling.toml
    grep -q '^ReadWritePaths=/var/lib/ling$' /lib/systemd/system/ling.service
    sudo grep -q '^dir = "/var/lib/ling/journal"$' /etc/ling/ling.toml
    sudo systemctl restart ling.service
    wait_for_service
    /usr/bin/ling-cli --lang en-US -c 'SET package-native int(42)'
    sudo test -s /var/lib/ling/journal/journal.lgj
    sudo sh -c 'printf "\n# preserved during package upgrade\n" >> /etc/ling/ling.toml'
    config_hash=$(sudo sha256sum /etc/ling/ling.toml)
    sudo touch /var/lib/ling/preserve-me
    sudo systemctl stop ling.service
    journal_hash=$(sudo sha256sum /var/lib/ling/journal/journal.lgj)
    install_package "$artifact" "$VERSION"
    assert_preserved
    remove_package
    if [ -n "${PREVIOUS_VERSION:-}" ]; then
        previous="${PREVIOUS_DIST:-target/previous-release}/ling_${PREVIOUS_VERSION}_linux_${ARCH}.$package"
        install_package "$previous" "$PREVIOUS_VERSION"
        assert_preserved
        install_package "$artifact" "$VERSION"
        assert_preserved
        remove_package
        if [ -n "${UPDATE_VERSION:-}" ]; then
            test "$UPDATE_VERSION" = "$VERSION"
            install_package "$previous" "$PREVIOUS_VERSION"
            sudo --preserve-env=LING_GITHUB_TOKEN /usr/bin/ling --lang en-US update --version "$UPDATE_VERSION"
            wait_for_service
            assert_versions "$VERSION"
            assert_preserved
            remove_package
        else
            echo 'ling update native upgrade: 未执行 / not executed (requires a published UPDATE_VERSION).'
        fi
    else
        echo 'Previous-version package upgrade and ling update: 未执行 / not executed (PREVIOUS_VERSION is unset).'
    fi
done
# 跳过服务的入口仍验证已选择的包类型，可用于真正的 RPM 主机。
# Verify service-skip behavior for the selected package type, including native RPM hosts.
if [ "$package" = deb ]; then
    sudo env LING_SKIP_SERVICE=1 dpkg -i "$artifact"
else
    sudo env LING_SKIP_SERVICE=1 rpm -U --replacepkgs "${rpm_options[@]}" "$artifact"
fi
if systemctl is-active --quiet ling.service; then exit 1; fi
remove_package
