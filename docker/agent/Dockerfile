FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG NODE_VERSION=24.14.1
ARG NODE_SHA256_AMD64=84d38715d449447117d05c3e71acd78daa49d5b1bfa8aacf610303920c3322be
ARG NODE_SHA256_ARM64=71e427e28b78846f201d4d5ecc30cb13d1508ca099ef3871889a1256c7d6f67e

# 先用 HTTP 华为源安装 ca-certificates，再切换为 HTTPS
RUN sed -i 's|http://archive.ubuntu.com|http://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && sed -i 's|http://security.ubuntu.com|http://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && sed -i 's|http://repo.huaweicloud.com|https://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && rm -rf /var/lib/apt/lists/*

# 安装基础工具
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    curl \
    wget \
    ripgrep \
    fd-find \
    jq \
    python3 \
    python3-venv \
    unzip \
    xz-utils \
    openssh-client \
    vim-tiny \
    tree \
    less \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# Vision MCP 通过 npx 在 agent 容器内运行。使用官方 Node 二进制，避免 Ubuntu 的
# npm 元包引入整套前端构建依赖，同时固定版本与多架构校验和。
RUN arch="$(dpkg --print-architecture)" \
    && case "$arch" in \
        amd64) node_arch=x64; node_sha256="${NODE_SHA256_AMD64}" ;; \
        arm64) node_arch=arm64; node_sha256="${NODE_SHA256_ARM64}" ;; \
        *) echo "unsupported Node.js architecture: $arch" >&2; exit 1 ;; \
    esac \
    && archive="node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
    && curl -fL --retry 5 --retry-delay 2 --connect-timeout 20 --max-time 180 \
        "https://nodejs.org/dist/v${NODE_VERSION}/${archive}" \
        -o "/tmp/${archive}" \
    && echo "${node_sha256}  /tmp/${archive}" | sha256sum -c - \
    && tar -xJf "/tmp/${archive}" -C /usr/local --strip-components=1 \
    && node --version \
    && npm --version \
    && npx --version \
    && rm -f "/tmp/${archive}"

# 创建工作目录
RUN mkdir -p /workspace
WORKDIR /workspace
