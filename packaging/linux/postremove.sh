#!/bin/sh
# 清理服务缓存，不删除用户配置、Journal 或服务账户。
# Refresh service metadata without deleting configuration, journals or the account.
set -eu
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
fi
