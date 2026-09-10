#!/usr/bin/env bash
# 转发到 skill 包内的真实安装器(单一源), 保留 git clone 后 `./install.sh` 的用法。
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/skills/causal-memory/install.sh" "$@"
