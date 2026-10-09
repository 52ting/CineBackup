#!/usr/bin/env bash
#
# 魔王拷贝（MowangCopy）—— macOS 签名 + 公证 + 盖章 脚本
#
# 背景：build_macos.sh 打出的 .app / .dmg 是「未签名」的，分发给别人的 Mac 时
#       会被 Gatekeeper 拦截，只能靠手动 xattr -dr com.apple.quarantine 放行。
#       本脚本在你有 Apple 开发者账号之后，给产物补上「签名 + 公证」，就能直接分发。
#
# 用法（先改下面的三个占位变量，再执行）：
#   DEVELOPER_ID="Developer ID Application: 你的名字 (TEAMID)"
#   APPLE_ID="你的 Apple ID 邮箱"
#   APPLE_TEAM_ID="你的 Team ID（十位，形如 ABCDEFGH12）"
#   bash tools/sign_notarize.sh [path/to/魔王拷贝.app]
#
# 前置条件：
#   1) 已安装 Xcode Command Line Tools（xcode-select --install）
#   2) 已用 Xcode / Apple Developer 后台 申请「Developer ID Application」证书
#      （钥匙串里能看到，证书名形如 “Developer ID Application: xxx (TEAMID)”）
#   3) 已在 Apple ID 开启 App 专用密码（Apple ID 网页 → 登录与安全 → App 专用密码），
#      公证上传时用「专用密码」而不是登录密码
#   4) 目标 .app 已由 build_macos.sh 打出（未签名即可，本脚本会覆盖签名）
#
# 说明：
#   - 本脚本只签名「Developer ID Application」证书（面向 Mac 应用商店之外的分发），
#     与 App Store 上架（需 Developer ID + 审核）是两条路。
#   - 公证成功后，首次打开仍需用户允许一次（「打开方式」→ 右键打开 或 系统设置放行），
#     但不会再被“已损坏 / 无法验证开发者”拦死。
#   - 全部命令需在 macOS 上执行；GitHub Actions 免费 runner 也能跑（见 build-macos.yml 注释）。

set -eu

cd "$(dirname "$0")/.."

say()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
die()  { printf '\n\033[1;31m[×] %s\033[0m\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- 占位配置（改这里）
# 钥匙串里证书的完整名字。查看方式：
#   security find-identity -v -p codesigning
DEVELOPER_ID="${DEVELOPER_ID:-Developer ID Application: 你的名字 (TEAMID)}"
# 你的 Apple ID（公证上传用，配合 App 专用密码）
APPLE_ID="${APPLE_ID:-you@example.com}"
# Apple 开发者 Team ID（十位大写字母数字）
APPLE_TEAM_ID="${APPLE_TEAM_ID:-ABCDEFGH12}"
# App 专用密码：在 Apple ID 网站生成，或运行下面的命令时按提示输入
# （建议不要写死在脚本里，用环境变量 APP_SPECIFIC_PASSWORD 或交互输入）
APP_SPECIFIC_PASSWORD="${APP_SPECIFIC_PASSWORD:-}"

# ---------------------------------------------------------------- 1) 环境检查

say "环境自检"
[ "$(uname -s)" = "Darwin" ] || die "本脚本只能在 macOS 上跑（当前是 $(uname -s)）。"

if ! xcode-select -p >/dev/null 2>&1; then
  die "缺少 Xcode Command Line Tools。先执行：xcode-select --install"
fi
command -v codesign >/dev/null 2>&1 || die "找不到 codesign（请确认已装 Xcode CLT）。"
command -v xcrun >/dev/null 2>&1 || die "找不到 xcrun。"

if [ "$DEVELOPER_ID" = "Developer ID Application: 你的名字 (TEAMID)" ]; then
  die "还没填 DEVELOPER_ID 占位值。先运行：
  security find-identity -v -p codesigning
把输出里的证书完整名字填到本脚本 DEVELOPER_ID 变量。"
fi
security find-identity -v -p codesigning | grep -F "$DEVELOPER_ID" >/dev/null 2>&1 \
  || die "钥匙串里找不到证书「$DEVELOPER_ID」，请先安装并确认名称一致。"

# ---------------------------------------------------------------- 2) 定位产物

APP="${1:-}"
if [ -z "$APP" ]; then
  REL="src-tauri/target/universal-apple-darwin/release"
  [ -d "$REL/bundle/macos/魔王拷贝.app" ] || REL="src-tauri/target/release"
  APP="$REL/bundle/macos/魔王拷贝.app"
fi
[ -d "$APP" ] || die "找不到 .app：$APP
先跑  UNIVERSAL=1 bash tools/build_macos.sh  或  bash tools/build_macos.sh"

APP="$(cd "$(dirname "$APP")" && pwd)/$(basename "$APP")"
say "待签名产物：$APP"

# ---------------------------------------------------------------- 3) 签名（含嵌套组件）

say "签名（--deep 覆盖内嵌框架/插件，--force 允许覆盖未签名/旧签名）"
SIGN_CMD=(codesign --force --deep --timestamp --options runtime --sign "$DEVELOPER_ID")
if [ -f "src-tauri/entitlements.mac.plist" ]; then
  SIGN_CMD+=(--entitlements "src-tauri/entitlements.mac.plist")
fi
"${SIGN_CMD[@]}" "$APP"
codesign --verify --deep --strict --verbose=2 "$APP" || die "签名自检失败"

# ---------------------------------------------------------------- 4) 打包 dmg（如需要）

DMG_DIR="$(dirname "$APP")/../dmg"
DMG="$(ls "$DMG_DIR"/魔王拷贝_*.dmg 2>/dev/null | head -1 || true)"
if [ -n "$DMG" ]; then
  say "对现有 dmg 签名：$DMG"
  codesign --force --timestamp --options runtime --sign "$DEVELOPER_ID" "$DMG"
  codesign --verify --strict --verbose=2 "$DMG"
else
  say "没找到现成 dmg，跳过 dmg 签名（.app 已签名，可直接 zip 分发或用 Disk Utility 打包）。"
fi

# ---------------------------------------------------------------- 5) 公证（Notary）

if [ -z "$APP_SPECIFIC_PASSWORD" ]; then
  say "公证上传需要 App 专用密码，请输入（输入不回显）："
  read -r -s APP_SPECIFIC_PASSWORD
fi
[ -n "$APP_SPECIFIC_PASSWORD" ] || die "没有 App 专用密码，无法公证。可在 Apple ID 网页生成后：
  export APP_SPECIFIC_PASSWORD='xxxx-xxxx-xxxx-xxxx'
再重跑本脚本。"

say "提交公证（--wait 会等结果，通常 1–5 分钟）"
xcrun notarytool submit "$APP" \
  --apple-id "$APPLE_ID" \
  --team-id "$APPLE_TEAM_ID" \
  --password "$APP_SPECIFIC_PASSWORD" \
  --wait \
  || die "公证失败（详见上方输出）。常见原因：专用密码错误 / 证书不在该 Team / 网络问题。"

# ---------------------------------------------------------------- 6) 盖章（Staple）

say "把公证票据钉进产物（离线也能通过验证）"
xcrun stapler staple "$APP" || die "staple 失败（公证已通过，可手动重试：xcrun stapler staple \"$APP\"）"
xcrun stapler validate "$APP"

# ---------------------------------------------------------------- 7) 收尾自检

say "最终校验"
spctl --assess --type execute --verbose=4 "$APP" && echo "✓ Gatekeeper 评估通过（可分发）"
codesign -dv --verbose=4 "$APP" 2>&1 | grep -E "Signature|TeamIdentifier" || true
echo ""
echo "────────────────────────────────────────────────────────────────"
echo "签名 + 公证完成。分发方式："
echo "  1) 直接发 .app 压缩包（zip -r 魔王拷贝.app.zip 魔王拷贝.app）"
echo "  2) 或先打 dmg 再对 dmg 执行 codesign + notarytool + stapler（本脚本第 4/5/6 步）"
echo "────────────────────────────────────────────────────────────────"
