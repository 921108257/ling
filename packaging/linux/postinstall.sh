#!/bin/sh
# 建立最小权限服务账户，保留已有配置和状态。
# Create a least-privilege service account and preserve existing configuration/state.
set -eu
getent group ling >/dev/null || groupadd --system ling
if ! getent passwd ling >/dev/null; then
    useradd --system --gid ling --home-dir /var/lib/ling --no-create-home --shell /sbin/nologin ling
fi
install -d -m 0750 -o root -g ling /etc/ling
install -d -m 0750 -o ling -g ling /var/lib/ling
if [ ! -e /etc/ling/ling.toml ]; then
    install -m 0640 -o root -g ling /usr/share/ling/ling.service.toml /etc/ling/ling.toml
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    # 无自动启动标记适用于离线映像和手动配置后启动。
    # The marker supports offline images and starting after manual configuration.
    if [ "${LING_SKIP_SERVICE:-0}" != 1 ] && [ ! -e /etc/ling/no-autostart ]; then
        systemctl enable ling.service
        systemctl restart ling.service
        # Type=simple 可能先成功后退出，拒绝立即启动失败。
        # Type=simple can exit just after activation; reject immediate startup failure.
        sleep 2
        if ! systemctl is-active --quiet ling.service; then
            # 保留启动诊断而不输出配置内容；Keep startup diagnostics without printing configuration.
            systemctl status ling.service --no-pager || true
            journalctl -u ling.service --no-pager -n 30 || true
            exit 1
        fi
    fi
fi
