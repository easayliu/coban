# ---------- 前端构建 ----------
FROM node:22-alpine AS frontend
WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml ./
RUN npm install -g pnpm@10 && pnpm install --frozen-lockfile
# vite.config.ts 从 Cargo.toml 读版本号注入 __APP_VERSION__（页脚显示），而这一阶段只拷
# admin-ui，于是配置加载即 ENOENT、整个前端构建挂掉。放在 COPY admin-ui 之前：
# Cargo.toml 变得远比前端源码少，这一层能一直命中缓存。
COPY Cargo.toml /app/Cargo.toml
COPY admin-ui ./
RUN pnpm build

# ---------- Rust 构建 ----------
# 用 Debian glibc：出站 TLS 走 reqwest 的默认档（native-tls），在 Linux 上就是**系统
# OpenSSL**，而这正是想要的——官方 codex 在 Linux 上跑出来的也是 OpenSSL 那份 ClientHello
# （它的 `TlsBackend` 默认档同样是 native-tls，见 Cargo.toml 里 reqwest 那段注）。换成 musl
# 静态链一份自带的 OpenSSL，指纹就又变成「只有 coban 才有的那一份」了。
#
#   - **pkg-config + libssl-dev**：openssl-sys 靠它们找到系统 OpenSSL；缺了报的是
#     `Could not find directory of OpenSSL installation`。
#   - **build-essential**：zstd-sys 要编一份 C 代码（解来访请求体的 zstd 用）。
FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# ---- 依赖预编译层 ----
# 只拷清单、用一个空 main 先把**依赖**编出来。这一层的失效条件仅是 Cargo.toml/Cargo.lock
# 变化，改业务代码不会动它，于是 OpenSSL/zstd 那几个 -sys crate 只在换依赖时才编一次。
#
# 空 main 编译不需要 admin-ui/dist：rust-embed 的宏在**我们自己的 crate** 里才展开，
# 这一层还没有真实的 src，轮不到它读目录。
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# ---- 真实构建 ----
COPY src ./src
# rust-embed 在编译期读取 admin-ui/dist（相对 crate 根 /app）。
COPY --from=frontend /app/admin-ui/dist ./admin-ui/dist
# 先删掉空 main 留下的产物：crate 名没变，不删的话有让 cargo 误判为「已是最新」的余地，
# 那会把一个空壳二进制打进镜像（起来就是 CMD 立刻退出，且不报错）。
RUN rm -f target/release/coban target/release/deps/coban-* \
    && cargo build --release

# ---------- 运行时 ----------
FROM debian:bookworm-slim
# libssl3：openssl-sys 是**动态**链接系统 OpenSSL 的，运行时得有 libssl.so.3 / libcrypto.so.3。
# 漏了的话表现是容器起不来而不是构建失败。ca-certificates 提供根证书——native-tls 读的是
# 系统信任库，不像 webpki-roots 那样把根证书编进二进制。
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/coban /usr/local/bin/coban

# 凭证持久化目录（挂载卷）
ENV COBAN_HOME=/app/config
VOLUME ["/app/config"]

EXPOSE 4700
# 容器内绑 0.0.0.0；默认即不自动开浏览器
CMD ["coban", "--host", "0.0.0.0", "--port", "4700"]
