#!/bin/sh
# 卸载时停止并注销，升级时交由 postinstall 重启。
# Stop and disable on removal; let postinstall restart during upgrades.
set -eu
case "${1:-}" in
    remove|0)
        if [ -d /run/systemd/system ]; then
            systemctl disable --now ling.service
        fi
        ;;
esac
