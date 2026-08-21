# coban

Codex 授权代理：用 ChatGPT 订阅账号登录，把多个账号的 token 存进本地 SQLite，
再把 Codex CLI 的请求转发到 `chatgpt.com/backend-api/codex`，按优先级与轮换选号。

架构与实现取向对齐同仓的 [luban](../luban)（Claude Code 那一侧的同类代理）。

## 它做什么

- **多账号**：一台机器管多个 ChatGPT 订阅账号，按优先级分档、同档内取最久未使用的那个。
- **只换鉴权**：请求体逐字节透传，只把来访的接入 Key 换成选中账号的 OAuth token
  与 `chatgpt-account-id`，响应流式原样回传。
- **额度感知**：解析上游的 `x-codex-*` 限流头，额度将满时自动把账号暂停到窗口重置；
  撞 429 则冷却一段时间并换号重试。
- **用量与计价**：从 SSE 的 `response.completed` 里嗅探 token 用量，按官方 API 价目估算
  等价花费（**不是账单**——订阅模式扣的是额度，这个数只用于横向比较各账号的消耗强度）。
- **逐账号出站代理**：每个账号可单独配 `socks5h://` / `http://` 出口，该账号的**全部**
  出站流量（转发、刷 token、连通性测试）都走它。
- **管理界面**：内嵌的 React 控制台，账号增删改、限流设置、用量明细都在里面。

## 快速开始

```bash
# 1. 构建前端（rust-embed 在编译期读取 admin-ui/dist，这步不能跳）
cd admin-ui && pnpm install && pnpm build && cd ..

# 2. 构建并启动
cargo run --release -- --api-key <你自己定的接入key>
```

打开 `http://127.0.0.1:4700/`，点「添加账号」，两条路任选：

- **浏览器授权**：打开 OpenAI 授权页，完成后浏览器会跳到
  `http://localhost:1455/auth/callback?code=…`。**这个地址打不开是正常的**——它是 Codex CLI
  监听的端口，coban 这边连不上。直接把地址栏里那条完整 URL 复制粘回页面即可。
- **导入 auth.json**：这台机器已经 `codex login` 过的话，把 `~/.codex/auth.json` 的内容
  整段贴进来。没有图形界面的服务器上这通常是唯一走得通的路。

然后把设置页给出的片段粘进 `~/.codex/config.toml`：

```toml
model_provider = "coban"

[model_providers.coban]
name = "coban"
base_url = "http://127.0.0.1:4700/v1"
wire_api = "responses"
env_key = "COBAN_API_KEY"
```

再 `export COBAN_API_KEY=<同一个接入key>`，`codex` 就走 coban 了。

## 命令行

```
coban                      # 启动网页服务 + 转发代理（默认 0.0.0.0:4700）
coban status               # 列出已保存的账号
coban logout               # 清空所有账号

  --host <HOST>            绑定地址（默认 0.0.0.0；只给本机用就写 127.0.0.1）
  --port <PORT>            端口（默认 4700）
  --api-key <KEY>          接入 Key，也可用 COBAN_API_KEY
  --admin-password <PW>    管理密码，也可用 COBAN_ADMIN_PASSWORD
  --open                   启动后自动打开浏览器
```

命令行/环境变量给的值**优先于网页设置，并让网页上那一项变成只读**。

数据库默认在 `~/.coban/coban.db`，`COBAN_HOME` 可改基目录。

## Docker

一键安装（拉镜像、写 compose、起容器，并打印接入步骤）：

```bash
curl -fsSL https://raw.githubusercontent.com/easayliu/coban/main/install.sh | bash
```

可用 `INSTALL_DIR` / `PORT` / `IMAGE_TAG` / `COBAN_API_KEY` / `COBAN_ADMIN_PASSWORD` /
`AUTO_START` 覆盖默认值，国内可用 `IMAGE_REG=ghcr.nju.edu.cn`。已 clone 仓库的话直接：

```bash
docker compose up -d
```

凭证库落在宿主机 `./config/`。镜像由 tag 触发的 CI 推到 GHCR。

## 两个安全前提

1. **不设 `--api-key` 时代理不校验来访身份**——任何能访问这个端口的人都能用你的账号发请求。
2. **不设管理密码时 `/api/*` 完全敞开**，包括读取接入 Key 本身。

只在可信的本机网络上省掉这两样；要对外暴露，两个都得设。

## 代码结构

| 文件 | 职责 |
|---|---|
| `src/main.rs` | CLI 与日志装配 |
| `src/config.rs` | Codex 的 OAuth 参数与上游端点（取自 codex v0.98.0 二进制内的字面量） |
| `src/oauth.rs` | PKCE、授权 URL、token 交换与刷新、id_token claim 解析 |
| `src/credentials.rs` | 凭证模型与刷新判定 |
| `src/store.rs` | SQLite 持久化、选号、限流窗口、用量账本 |
| `src/clients.rs` | 出站 HTTP 客户端与逐账号代理池 |
| `src/proxy.rs` | 转发、SSE 用量嗅探、限流头解析、账号级错误判定 |
| `src/web.rs` | 管理 JSON 接口与路由装配 |
| `src/auth.rs` | 管理界面鉴权 |
| `src/pricing.rs` | 按模型估算等价花费 |
| `src/admin_ui.rs` | 内嵌前端的静态服务 |
| `admin-ui/` | React + Vite + Tailwind 控制台 |

## 开发

```bash
cargo test                       # 后端单测
cargo fmt --all                  # 排版（规则见 rustfmt.toml）
cd admin-ui && pnpm dev          # 前端热更新，/api 代理到 127.0.0.1:4700
```

前端还带一个不连后端的离线预览页：`pnpm dev` 后打开 `/preview.html`，用一批手写账号
（正常 / 将满 / 限流 / 冷却 / 封禁 / 停用 / 无快照）跑生产同一套组件，用来验收卡片与列表的
排布。`?lang=en` 切英文、`?view=list` 看表格、`?state=loading` 看骨架屏。它不在 `index.html`
的入口里，因此不会被打进 `dist`，也就不会进二进制。
