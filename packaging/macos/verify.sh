#!/usr/bin/env bash
# 仅在一次性主机验收，保留系统、摘要、命令及结果。
# Validate on disposable hosts only; retain OS details, hashes, commands and results.
set -euo pipefail
: "${VERSION:?VERSION is required}"
: "${ARCH:?ARCH is required}"
if [ "${CI:-}" != true ] && [ "${LING_NATIVE_ACCEPTANCE:-}" != 1 ]; then
    echo 'Use a disposable host and set LING_NATIVE_ACCEPTANCE=1 explicitly.' >&2
    exit 1
fi
[ ! -e /usr/local/bin/ling ] || { echo 'A clean host without an existing ling installation is required.' >&2; exit 1; }
mkdir -p target/native-acceptance
exec > >(tee "target/native-acceptance/macos-${ARCH}.log") 2>&1
trap 'result=$?; set +x; printf "native_acceptance_exit_code=%s\n" "$result"' EXIT
set -x
date -u '+%Y-%m-%dT%H:%M:%SZ'
sw_vers
uname -a
printf 'candidate=%s previous=%s update_target=%s\n' "$VERSION" "${PREVIOUS_VERSION:-not-executed}" "${UPDATE_VERSION:-not-executed}"
artifact="dist/ling_${VERSION}_macos_${ARCH}.pkg"
tar -xOf "dist/ling_${VERSION}_macos_${ARCH}.tar.gz" ling/LICENSE | cmp LICENSE -
tar -xOf "dist/ling_${VERSION}_macos_${ARCH}.tar.gz" ling/THIRD-PARTY-NOTICES.txt > target/macos-portable-notices.txt
grep -q '^ring ' target/macos-portable-notices.txt
grep -q '^Rust standard library ' target/macos-portable-notices.txt
shasum -a 256 "dist/ling_${VERSION}_macos_${ARCH}.tar.gz"
wait_for_service() {
    for ((attempt=0; attempt<30; attempt++)); do
        if sudo launchctl print system/io.ling.cache >/dev/null 2>&1 && /usr/local/bin/ling-cli --lang en-US -c PING; then return; fi
        sleep 1
    done
    sudo launchctl print system/io.ling.cache
    return 1
}
assert_versions() {
    test "$(/usr/local/bin/ling --version)" = "ling $1"
    test "$(/usr/local/bin/ling-cli --version)" = "ling-cli $1"
}
install_package() {
    shasum -a 256 "$1"
    sudo installer -pkg "$1" -target /
    wait_for_service
    assert_versions "$2"
}
assert_preserved() {
    test "$(sudo shasum -a 256 /usr/local/etc/ling/ling.toml)" = "$config_hash"
    sudo test -f /usr/local/var/lib/ling/preserve-me
    test "$(sudo shasum -a 256 /usr/local/var/lib/ling/journal/journal.lgj)" = "$journal_hash"
}
remove_package() {
    sudo /usr/local/bin/ling-uninstall
    if sudo launchctl print system/io.ling.cache >/dev/null 2>&1; then return 1; fi
    test ! -e /Library/LaunchDaemons/io.ling.cache.plist
    assert_preserved
}
install_package "$artifact" "$VERSION"
# bootout / bootstrap 是 launchd 的停止与启动；kickstart -k 验证重启。
# Use launchd bootout / bootstrap for stop/start, then kickstart -k for restart.
sudo launchctl bootout system/io.ling.cache
if sudo launchctl print system/io.ling.cache >/dev/null 2>&1; then exit 1; fi
sudo launchctl bootstrap system /Library/LaunchDaemons/io.ling.cache.plist
wait_for_service
sudo launchctl kickstart -k system/io.ling.cache
wait_for_service
cmp LICENSE /usr/local/share/ling/LICENSE
grep -q '^ring ' /usr/local/share/ling/THIRD-PARTY-NOTICES.txt
grep -q '^Rust standard library ' /usr/local/share/ling/THIRD-PARTY-NOTICES.txt
# 在打包配置的目录中保留真实 Journal，而不以空标记文件代替。
# Preserve a real journal in the packaged configuration path rather than an empty marker alone.
sudo sed -i '' -e 's/^mode = "fast"$/mode = "reliable"/' -e 's/^# \[event_journal\]$/[event_journal]/' -e 's|^# dir = "/usr/local/var/lib/ling/journal"$|dir = "/usr/local/var/lib/ling/journal"|' /usr/local/etc/ling/ling.toml
sudo launchctl kickstart -k system/io.ling.cache
wait_for_service
/usr/local/bin/ling-cli --lang en-US -c 'SET package-native int(42)'
sudo test -s /usr/local/var/lib/ling/journal/journal.lgj
sudo sh -c 'printf "\n# preserved during package upgrade\n" >> /usr/local/etc/ling/ling.toml'
config_hash=$(sudo shasum -a 256 /usr/local/etc/ling/ling.toml)
sudo touch /usr/local/var/lib/ling/preserve-me
sudo launchctl bootout system/io.ling.cache
journal_hash=$(sudo shasum -a 256 /usr/local/var/lib/ling/journal/journal.lgj)
install_package "$artifact" "$VERSION"
assert_preserved
remove_package
if [ -n "${PREVIOUS_VERSION:-}" ]; then
    previous="${PREVIOUS_DIST:-target/previous-release}/ling_${PREVIOUS_VERSION}_macos_${ARCH}.pkg"
    install_package "$previous" "$PREVIOUS_VERSION"
    assert_preserved
    install_package "$artifact" "$VERSION"
    assert_preserved
    remove_package
    if [ -n "${UPDATE_VERSION:-}" ]; then
        test "$UPDATE_VERSION" = "$VERSION"
        install_package "$previous" "$PREVIOUS_VERSION"
        sudo --preserve-env=LING_GITHUB_TOKEN /usr/local/bin/ling --lang en-US update --version "$UPDATE_VERSION"
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
sudo touch /usr/local/etc/ling/no-autostart
shasum -a 256 "$artifact"
sudo installer -pkg "$artifact" -target /
if sudo launchctl print system/io.ling.cache >/dev/null 2>&1; then exit 1; fi
remove_package
