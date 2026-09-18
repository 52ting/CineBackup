#!/usr/bin/env bash
#
# CineBackup —— macOS 一键打包（产出 .app + .dmg）
#
#   bash tools/build_macos.sh                 # 本机架构，app + dmg
#   UNIVERSAL=1 bash tools/build_macos.sh     # 通用二进制（Intel + Apple Silicon）
#   BUNDLES=dmg bash tools/build_macos.sh     # 只要 dmg
#
# 说明：Tauri 不支持跨平台交叉编译，macOS 安装包必须在 macOS 上打。
#       本脚本只做「检查环境 → 装依赖 → 调 tauri build」，不做任何签名 / 公证。
#
# 注意：全脚本保持 bash 3.2 兼容（macOS 自带 /bin/bash 是 3.2），
#       所以不用关联数组、不展开空数组、不用 mapfile。

set -eu

cd "$(dirname "$0")/.."

say()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33m[!] %s\033[0m\n' "$*"; }
die()  { printf '\n\033[1;31m[×] %s\033[0m\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- 1) 环境检查

say "环境自检"

[ "$(uname -s)" = "Darwin" ] || die "本脚本只能在 macOS 上跑（当前是 $(uname -s)）。"

if ! xcode-select -p >/dev/null 2>&1; then
  die "缺少 Xcode Command Line Tools。先执行：xcode-select --install  然后再跑本脚本。"
fi

# rustup 装完不一定进当前 shell 的 PATH，主动 source 一次
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
command -v cargo >/dev/null 2>&1 || die "未安装 Rust：
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  装完重开终端，或先执行 . \"\$HOME/.cargo/env\""

command -v npm >/dev/null 2>&1 || die "未安装 Node.js（≥18）。推荐：brew install node"

echo "rustc : $(rustc -V)"
echo "cargo : $(cargo -V)"
echo "node  : $(node -v)"
echo "npm   : $(npm -v)"
echo "架构  : $(uname -m)"

# ---------------------------------------------------------------- 2) 图标兜底

if [ ! -f src-tauri/icons/icon.icns ]; then
  warn "src-tauri/icons/icon.icns 不存在，用 icon.png 现场生成一整套"
  [ -f src-tauri/icons/icon.png ] || die "连 icon.png 都没有，无法生成图标"
  npm run tauri icon ./src-tauri/icons/icon.png
fi

# ---------------------------------------------------------------- 3) 依赖

if [ ! -d node_modules ]; then
  say "安装前端依赖"
  npm install
else
  say "node_modules 已存在，跳过 npm install（要强制重装就先删掉它）"
fi

# ---------------------------------------------------------------- 4) 打包

say "开始打包（首次编译 Tauri 依赖约 3–10 分钟，之后增量很快）"

if [ "${UNIVERSAL:-0}" = "1" ]; then
  say "通用二进制模式：补齐两个 Rust target"
  rustup target add x86_64-apple-darwin aarch64-apple-darwin
  if [ -n "${BUNDLES:-}" ]; then
    npm run tauri build -- --target universal-apple-darwin --bundles "$BUNDLES"
  else
    npm run tauri build -- --target universal-apple-darwin
  fi
else
  if [ -n "${BUNDLES:-}" ]; then
    npm run tauri build -- --bundles "$BUNDLES"
  else
    npm run tauri build
  fi
fi

# ---------------------------------------------------------------- 5) 产物

say "产物清单"

if [ "${UNIVERSAL:-0}" = "1" ]; then
  REL="src-tauri/target/universal-apple-darwin/release"
else
  REL="src-tauri/target/release"
fi

echo "[bundle]"
ls -lh "$REL/bundle/macos" 2>/dev/null || echo "  （没有 .app）"
ls -lh "$REL/bundle/dmg" 2>/dev/null || echo "  （没有 .dmg）"

APP="$REL/bundle/macos/CineBackup.app"

echo ""
echo "────────────────────────────────────────────────────────────────"
echo "装到本机（未签名，首次打开会被 Gatekeeper 拦，必须去掉隔离属性）："
echo ""
echo "  cp -R \"$APP\" /Applications/"
echo "  xattr -dr com.apple.quarantine /Applications/CineBackup.app"
echo "  open /Applications/CineBackup.app"
echo ""
echo "分发给别人的 Mac：把 $REL/bundle/dmg 里的 .dmg 发过去，"
echo "对方拖进「应用程序」后，同样要跑一次那条 xattr 命令。"
echo ""
echo "首次备份到外置盘 / 桌面 / 文档时，系统可能弹「想访问…」的授权框 —— 允许即可。"
echo "往 NTFS 盘写需要装第三方驱动（Paragon / Tuxera / Mounty），macOS 原生 NTFS 只读。"
echo "────────────────────────────────────────────────────────────────"
