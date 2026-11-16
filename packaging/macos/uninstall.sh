#!/bin/sh
# 只移除本产品安装的文件，保留用户配置、状态和账户。
# Remove only product files; retain user configuration, state and service account.
set -eu
[ "$(id -u)" -eq 0 ] || { echo 'Run with sudo' >&2; exit 1; }
if launchctl print system/io.ling.cache >/dev/null 2>&1; then
    launchctl bootout system/io.ling.cache
fi
launchctl disable system/io.ling.cache
rm -f /Library/LaunchDaemons/io.ling.cache.plist /usr/local/bin/ling /usr/local/bin/ling-cli /usr/local/bin/ling-uninstall
if [ -f /usr/local/share/ling/installed-docs.txt ]; then
    while IFS= read -r file; do
        case "$file" in
            /usr/local/share/ling/docs/*.md) rm -f -- "$file" ;;
        esac
    done < /usr/local/share/ling/installed-docs.txt
fi
for file in README.md ling.example.toml ling.service.toml installed-docs.txt; do
    rm -f "/usr/local/share/ling/$file"
done
pkgutil --forget io.ling.pkg
echo 'Removed ling. Configuration and state remain in /usr/local/etc/ling and /usr/local/var/lib/ling.'
