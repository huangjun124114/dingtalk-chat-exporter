#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_DIR="$(cd -- "$PROJECT_DIR/.." && pwd)"
OUTPUT_APP="${1:-$WORKSPACE_DIR/artifacts/钉钉群聊导出器.app}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_DIR/target}"
BINARY="$TARGET_DIR/release/dingtalk-chat-exporter"
SOURCE_ICON="$PROJECT_DIR/icons/icon.png"
SOURCE_PLIST="$PROJECT_DIR/packaging/macos/Info.plist"
PRIVACY_REMAP_FLAG="--remap-path-prefix=${HOME:?HOME 未设置}=/home/builder"

if [[ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]]; then
  ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS}"$'\x1f'"$PRIVACY_REMAP_FLAG"
else
  ENCODED_RUSTFLAGS="$PRIVACY_REMAP_FLAG"
fi

case "$OUTPUT_APP" in
  /*.app) ;;
  *)
    echo "错误：输出路径必须是绝对的 .app 路径：$OUTPUT_APP" >&2
    exit 2
    ;;
esac

for command_name in cargo sips plutil codesign; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "错误：缺少打包命令 $command_name" >&2
    exit 1
  fi
done

CARGO_ENCODED_RUSTFLAGS="$ENCODED_RUSTFLAGS" cargo build \
  --manifest-path "$PROJECT_DIR/Cargo.toml" \
  --locked \
  --release \
  --offline

if [[ ! -x "$BINARY" ]]; then
  echo "错误：Release 二进制不存在或不可执行：$BINARY" >&2
  exit 1
fi

STAGING_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dingtalk-chat-exporter-package.XXXXXX")"
STAGING_APP="$STAGING_DIR/钉钉群聊导出器.app"
PREVIOUS_APP="$STAGING_DIR/previous.app"

cleanup() {
  rm -rf -- "$STAGING_DIR"
}
trap cleanup EXIT

mkdir -p "$STAGING_APP/Contents/MacOS" "$STAGING_APP/Contents/Resources"
cp "$BINARY" "$STAGING_APP/Contents/MacOS/dingtalk-chat-exporter"
cp "$SOURCE_PLIST" "$STAGING_APP/Contents/Info.plist"
chmod 755 "$STAGING_APP/Contents/MacOS/dingtalk-chat-exporter"

sips -s format icns "$SOURCE_ICON" \
  --out "$STAGING_APP/Contents/Resources/AppIcon.icns" >/dev/null

plutil -lint "$STAGING_APP/Contents/Info.plist" >/dev/null
codesign --force --deep --sign - \
  --identifier com.ainuoyan.dingtalk-chat-exporter \
  "$STAGING_APP"
codesign --verify --deep --strict --verbose=2 "$STAGING_APP"

mkdir -p "$(dirname -- "$OUTPUT_APP")"
if [[ -e "$OUTPUT_APP" ]]; then
  mv "$OUTPUT_APP" "$PREVIOUS_APP"
fi
if ! mv "$STAGING_APP" "$OUTPUT_APP"; then
  if [[ -e "$PREVIOUS_APP" ]]; then
    mv "$PREVIOUS_APP" "$OUTPUT_APP"
  fi
  exit 1
fi

echo "macOS 应用已生成并通过严格签名校验：$OUTPUT_APP"
