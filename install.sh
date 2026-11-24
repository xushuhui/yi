#!/bin/sh
# yi 安装脚本：从 GitHub Releases 下载对应平台编译好的二进制。
#
#   curl -fsSL https://raw.githubusercontent.com/xushuhui/yi/main/install.sh | sh
#
# 可用的环境变量：
#   YI_VERSION       指定版本（v0.1.0 或 0.1.0），默认 latest
#   YI_INSTALL_DIR   安装目录，默认 ~/.local/bin
#   YI_BASE_URL      换一个发布源（镜像/自建），默认 GitHub Releases
set -eu

BASE_URL="${YI_BASE_URL:-https://github.com/xushuhui/yi/releases}"
VERSION="${YI_VERSION:-latest}"
INSTALL_DIR="${YI_INSTALL_DIR:-$HOME/.local/bin}"

os=$(uname -s)
arch=$(uname -m)

case "$os:$arch" in
  Darwin:arm64) target=aarch64-apple-darwin ;;
  Darwin:x86_64) target=x86_64-apple-darwin ;;
  Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
  *)
    echo "yi: 没有为 $os $arch 提供二进制。" >&2
    echo "    可以改用源码安装：cargo install --path .（需要 Rust）" >&2
    exit 1
    ;;
esac

if [ "$VERSION" = "latest" ]; then
  base="$BASE_URL/latest/download"
else
  case "$VERSION" in
    v*) ;;
    *) VERSION="v$VERSION" ;;
  esac
  base="$BASE_URL/download/$VERSION"
fi

asset="yi-$target"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "yi: 下载 $base/$asset"
curl -fL --retry 3 --proto '=https,file' -o "$tmp/$asset" "$base/$asset"

if curl -fsL --retry 3 --proto '=https,file' -o "$tmp/$asset.sha256" "$base/$asset.sha256" 2>/dev/null; then
  expected=$(cut -d' ' -f1 "$tmp/$asset.sha256")
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
  else
    actual=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
  fi
  if [ "$expected" != "$actual" ]; then
    echo "yi: 校验失败，下载的文件不完整或被改动过。" >&2
    exit 1
  fi
  echo "yi: 校验通过"
else
  echo "yi: 没找到校验文件，跳过校验。" >&2
fi

mkdir -p "$INSTALL_DIR"
chmod +x "$tmp/$asset"
mv "$tmp/$asset" "$INSTALL_DIR/yi"

installed=$("$INSTALL_DIR/yi" -v 2>/dev/null || echo "版本未知")
echo "yi: 已安装 $INSTALL_DIR/yi（${installed}）"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "yi: 提示：$INSTALL_DIR 不在 PATH 里，把它加进去。" >&2 ;;
esac

if ! command -v rg >/dev/null 2>&1; then
  echo "yi: 提示：没找到 rg（ripgrep），grep 和 glob 工具需要它。" >&2
fi
