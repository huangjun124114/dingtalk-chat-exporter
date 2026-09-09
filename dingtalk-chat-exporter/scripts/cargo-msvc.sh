#!/usr/bin/env bash
# 在 Git Bash 中运行 cargo 命令，自动配置 MSVC + Windows SDK 环境
# 用法: ./scripts/cargo-msvc.sh <cargo args...>
# 例:   ./scripts/cargo-msvc.sh test --lib cron
set -e

MSVC="/c/Program Files (x86)/Microsoft Visual Studio/18/BuildTools/VC/Tools/MSVC/14.50.35717"
SDK="/c/Program Files (x86)/Windows Kits/10"
SDKVER="10.0.26100.0"

export PATH="$MSVC/bin/Hostx64/x64:$PATH"
export INCLUDE="$(cygpath -w "$MSVC/include");$(cygpath -w "$SDK/Include/$SDKVER/ucrt");$(cygpath -w "$SDK/Include/$SDKVER/um");$(cygpath -w "$SDK/Include/$SDKVER/shared")"
export LIB="$(cygpath -w "$MSVC/lib/x64");$(cygpath -w "$SDK/Lib/$SDKVER/ucrt/x64");$(cygpath -w "$SDK/Lib/$SDKVER/um/x64")"

cd "$(dirname "$0")/.."
cargo "$@"
