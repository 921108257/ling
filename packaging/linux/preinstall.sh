#!/bin/sh
# 覆盖程序前停止已有官方服务；不修改配置或禁用状态。
# Stop the existing official service before replacing programs; preserve configuration and enablement.
set -eu
if [ -d /run/systemd/system ] && [ "$(systemctl show --property=LoadState --value ling.service)" = loaded ]; then
    systemctl stop ling.service
fi
