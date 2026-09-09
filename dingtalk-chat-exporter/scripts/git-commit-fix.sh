#!/usr/bin/env bash
# git-commit-fix.sh —— 规避此环境 git commit 不更新 loose ref 的 bug
#
# 背景：本机 PortableGit 2.55 在 commit 后常不把新提交写入 .git/refs/heads/<branch>
# （导致 "your current branch does not have any commits yet"），但 .git/logs/HEAD
# reflog 总是正确记录新哈希。本脚本：commit → 从 reflog 读新哈希 → 写回 loose ref → push。
#
# 用法：
#   git-commit-fix.sh "提交信息(字符串或信息文件路径)" [分支名] [是否push:1/0]
# 前置：已 git add 待提交内容。
# 说明：第一参数若为已存在文件则按 -F 文件提交，否则按 -m 字符串提交。

set -euo pipefail

MSG_ARG="${1:?需要提交信息（字符串或文件路径）}"
BRANCH="${2:-feat/scheduled-export}"
DO_PUSH="${3:-1}"

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# 记录 commit 前的 HEAD（可能为空/unborn）
BEFORE_REFLOG="$(tail -n 1 .git/logs/HEAD 2>/dev/null | awk '{print $2}' || echo '')"

# 暂存区为空则直接报错退出（避免静默产生空提交）
if [ -z "$(git diff --cached --name-only)" ]; then
    echo "ERROR: 暂存区为空，请先 git add 待提交内容" >&2
    exit 1
fi

# 执行提交（允许 ref 不落盘）；按入参是文件还是字符串选择 -F / -m
if [ -f "$MSG_ARG" ]; then
    git -c user.name="huangjun124" -c user.email="176446481@qq.com" commit -F "$MSG_ARG" >/dev/null 2>&1 || true
else
    git -c user.name="huangjun124" -c user.email="176446481@qq.com" commit -m "$MSG_ARG" >/dev/null 2>&1 || true
fi

# 从 reflog 末尾读取本次提交的新哈希
NEW="$(tail -n 1 .git/logs/HEAD | awk '{print $2}')"
if [ -z "$NEW" ] || [ "$NEW" = "0000000000000000000000000000000000000000" ]; then
    echo "ERROR: 无法从 reflog 解析新提交哈希" >&2
    exit 1
fi
if [ "$NEW" = "$BEFORE_REFLOG" ]; then
    echo "ERROR: reflog 未推进，提交未真正产生（检查 git add / 提交信息文件路径）" >&2
    exit 1
fi

# 校验提交对象存在且可解析
git cat-file -e "${NEW}^{commit}"

# 写回 loose ref（分支名含斜杠需确保子目录存在）
REF_PATH=".git/refs/heads/${BRANCH}"
mkdir -p "$(dirname "$REF_PATH")"
printf '%s\n' "$NEW" > "$REF_PATH"

# 确认 HEAD 解析正确
RESOLVED="$(git rev-parse HEAD)"
if [ "$RESOLVED" != "$NEW" ]; then
    echo "ERROR: 写回 ref 后 HEAD=$RESOLVED 仍不等于 NEW=$NEW" >&2
    exit 1
fi

echo "提交成功: $NEW"
git log --oneline -3

if [ "$DO_PUSH" = "1" ]; then
    echo "== 推送到 origin/$BRANCH =="
    git push origin "$BRANCH" 2>&1 | tail -3
fi
