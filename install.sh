#!/usr/bin/env bash
#
# coban 一键安装脚本（Docker 版）
#
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/easayliu/coban/main/install.sh | bash
#   bash install.sh
#
# 环境变量：
#   INSTALL_DIR           安装目录，默认 ~/coban
#   IMAGE_OWNER           镜像 owner，默认 easayliu
#   IMAGE_TAG             镜像 tag，默认 latest（由 tag 触发的 CI 构建产出）
#   IMAGE_REG             镜像 registry，默认 ghcr.io；国内可用 ghcr.nju.edu.cn
#   PORT                  宿主机监听端口，默认 4700
#   COBAN_API_KEY         接入用 API Key，默认留空（改由网页「客户端接入」页管理）
#   COBAN_ADMIN_PASSWORD  管理密码，默认留空（改由网页「控制台安全」页管理）
#   AUTO_START            安装后是否立即启动，默认 yes
#

set -euo pipefail

INSTALL_DIR="${INSTALL_DIR:-$HOME/coban}"
IMAGE_OWNER="${IMAGE_OWNER:-easayliu}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
IMAGE_REG="${IMAGE_REG:-ghcr.io}"
PORT="${PORT:-4700}"
COBAN_API_KEY="${COBAN_API_KEY:-}"
COBAN_ADMIN_PASSWORD="${COBAN_ADMIN_PASSWORD:-}"
AUTO_START="${AUTO_START:-yes}"

RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; BLUE=$'\033[34m'; BOLD=$'\033[1m'; RESET=$'\033[0m'

info()  { printf '%s[info]%s %s\n'  "$BLUE"   "$RESET" "$*"; }
warn()  { printf '%s[warn]%s %s\n'  "$YELLOW" "$RESET" "$*"; }
error() { printf '%s[error]%s %s\n' "$RED"    "$RESET" "$*" >&2; }
ok()    { printf '%s[ok]%s %s\n'    "$GREEN"  "$RESET" "$*"; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || { error "缺少依赖：$1，请先安装"; exit 1; }
}

detect_compose() {
  if docker compose version >/dev/null 2>&1; then
    echo "docker compose"
  elif command -v docker-compose >/dev/null 2>&1; then
    echo "docker-compose"
  else
    error "未检测到 docker compose / docker-compose"
    exit 1
  fi
}

main() {
  require_cmd docker
  local COMPOSE
  COMPOSE="$(detect_compose)"
  ok "docker 就绪；compose 命令：$COMPOSE"

  mkdir -p "$INSTALL_DIR/config"
  info "安装目录：$INSTALL_DIR"

  # ---------- docker-compose.yml ----------
  local COMPOSE_PATH="$INSTALL_DIR/docker-compose.yml"
  cat > "$COMPOSE_PATH" <<EOF
services:
  coban:
    image: ${IMAGE_REG}/${IMAGE_OWNER}/coban:${IMAGE_TAG}
    container_name: coban
    init: true
    extra_hosts:
      - "host.docker.internal:host-gateway"
    ports:
      - "${PORT}:4700"
    environment:
      - COBAN_API_KEY=${COBAN_API_KEY}
      - COBAN_ADMIN_PASSWORD=${COBAN_ADMIN_PASSWORD}
    volumes:
      - ./config/:/app/config/
    restart: unless-stopped
EOF
  ok "已写入 $COMPOSE_PATH"

  if [[ "$AUTO_START" != "yes" ]]; then
    info "AUTO_START=no，跳过启动"
    print_summary
    return
  fi

  (
    cd "$INSTALL_DIR"
    info "拉取镜像 ${IMAGE_REG}/${IMAGE_OWNER}/coban:${IMAGE_TAG} ..."
    $COMPOSE pull
    info "启动容器 ..."
    $COMPOSE up -d
  )

  ok "启动完成"
  print_summary
}

print_summary() {
  cat <<EOF

${BOLD}${GREEN}✓ coban 安装完成${RESET}

  目录:      ${INSTALL_DIR}
  网页:      http://127.0.0.1:${PORT}/

后续步骤（浏览器打开上面的网页）:
  1. 「添加账号」用 ChatGPT 订阅账号接入（可加多个），两条路任选：
     · 浏览器授权：完成后浏览器会跳到 localhost:1455 的回调地址，${BOLD}那个地址打不开是正常的${RESET}，
       把地址栏里那条完整 URL 复制粘回页面即可。
     · 导入 auth.json：这台机器 ${BOLD}codex login${RESET} 过的话，把 ~/.codex/auth.json 整段贴进来。
       没有图形界面的服务器上这通常是唯一走得通的路。
  2. 「客户端接入」生成/填写接入 Key（或用 COBAN_API_KEY 环境变量）

Codex 接入 —— 把下面这段加进 ${BOLD}~/.codex/config.toml${RESET}:

  model_provider = "coban"

  [model_providers.coban]
  name = "coban"
  base_url = "http://127.0.0.1:${PORT}/v1"
  wire_api = "responses"
  env_key = "COBAN_API_KEY"

再 ${BOLD}export COBAN_API_KEY=<接入设置里的 Key>${RESET}，codex 就走 coban 了。

  ${YELLOW}env_key 这一项不能换成 http_headers${RESET}：codex 的 auth manager 会在 http_headers
  之后覆盖 Authorization，塞它自己的 ChatGPT token，coban 只会看到一个不匹配的 Key 并回 401。

  ${YELLOW}ChatGPT/Codex 桌面端从 Dock 启动读不到 shell 的环境变量${RESET}，要额外注入一次：
    macOS   ${BOLD}launchctl setenv COBAN_API_KEY <Key>${RESET}   然后完全退出应用再打开
    撤销    ${BOLD}launchctl unsetenv COBAN_API_KEY${RESET}

  模型名要用上游认的 slug（gpt-5.6-sol / gpt-5.6-terra / gpt-5.6-luna 之类）。写 "gpt-5"
  会被上游直接回 400: ${BOLD}not supported when using Codex with a ChatGPT account${RESET}。
  当前账号支持哪些，在账号卡片的「模型」里能拉到。

常用命令（在 ${INSTALL_DIR} 目录下执行）:
  查看日志   ${BOLD}docker compose logs -f${RESET}
  停止       ${BOLD}docker compose down${RESET}
  升级       ${BOLD}docker compose pull && docker compose up -d${RESET}

  凭证库持久化在 ${INSTALL_DIR}/config/（重启不丢）。
  远程服务器登录：本机 ${BOLD}ssh -L ${PORT}:127.0.0.1:${PORT} <user>@<server>${RESET} 后访问上面的网页。

  ${YELLOW}两个安全前提${RESET}：不设 COBAN_API_KEY 时代理不校验来访身份；不设 COBAN_ADMIN_PASSWORD
  时 /api/* 完全敞开（包括读取接入 Key 本身）。要对外暴露，两个都得设。

EOF
}

main "$@"
