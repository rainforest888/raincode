#!/usr/bin/env bash
# Raincode 安装脚本(macOS / Linux)
# 用法: bash install.sh
# 效果:构建 release → 复制 raincode 到 ~/.raincode/bin → 加入 shell PATH。
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
BIN="$HOME/.raincode/bin"
EXE="$REPO/target/release/raincode"

# 1) 确保 release 存在
if [ ! -x "$EXE" ]; then
  echo ">>> 构建 release(首次较慢)..."
  (cd "$REPO" && cargo build --release)
fi

# 2) 复制到 ~/.raincode/bin
mkdir -p "$BIN"
cp "$EXE" "$BIN/raincode"
chmod +x "$BIN/raincode"

# 3) 加入 PATH(幂等,按 shell 判断)
SHELL_RC="${SHELL_RC:-}"
if [ -z "$SHELL_RC" ]; then
  case "${SHELL:-}" in
    *zsh) SHELL_RC="$HOME/.zshrc" ;;
    *bash) SHELL_RC="$HOME/.bashrc" ;;
    *) SHELL_RC="$HOME/.profile" ;;
  esac
fi
if ! echo "$PATH" | grep -q "$BIN"; then
  echo "export PATH=\"$BIN:\$PATH\"" >> "$SHELL_RC"
  echo "✔ 已把 $BIN 加入 $SHELL_RC(新开终端生效)"
fi

echo ""
echo "Raincode 安装完成!"
echo "  新终端里输入:  raincode repl      (交互式 TUI)"
echo "               raincode run '写个 hello world'"
echo "  首次使用请先:  raincode setup"
