#!/usr/bin/env bash
# 交付前检查：fmt → clippy（严格）→ 全量测试，可选装到 ~/.local。
#
# 这是本地与 CI 共用的唯一入口：AGENTS.md 只写这一条命令，
# 不在别处手拼 cargo 管道（容易把失败过滤掉，也会让每次检查长得不一样）。
#
# 用法：
#   scripts/check.sh              全套
#   scripts/check.sh fast         只做 fmt + clippy（CI 之外的快速自查）
#   scripts/check.sh --install    全套跑完后安装到 ~/.local/bin
set -euo pipefail
cd "$(dirname "$0")/.."

fast=0
install=0
for arg in "$@"; do
    case "$arg" in
        fast) fast=1 ;;
        --install) install=1 ;;
        *)
            echo "未知参数：$arg（可选 fast / --install）" >&2
            exit 2
            ;;
    esac
done

started=$SECONDS
step() {
    printf '\n== %s\n' "$*"
}

step "格式"
cargo fmt --check

step "静态检查（clippy，警告即失败）"
cargo clippy --all-targets --locked -- -D warnings

if [ "$fast" -eq 0 ]; then
    step "测试"
    cargo test --locked
fi

if [ "$install" -eq 1 ]; then
    step "安装到 ~/.local/bin"
    cargo install --path . --root "$HOME/.local" --force --offline
fi

printf '\n全部通过（%s 秒）\n' "$((SECONDS - started))"
