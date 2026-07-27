FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG GH_VERSION=2.92.0
ARG NODE_VERSION=24.14.1
ARG GH_SHA256_AMD64=8f8212b1a9cec261a8839e0893168f50d3fc70f095da257feef4229234cefdf8
ARG GH_SHA256_ARM64=34d620b7c884774ed86236541535170889fda0b99aafbdab8b69c7d458b5ca6b
ARG GH_SHA256_ARMV6=acf889527e8351f056bcedf959d9a4c011d4788ed89eb0ba6ca46bdb137b471d
ARG NODE_SHA256_AMD64=84d38715d449447117d05c3e71acd78daa49d5b1bfa8aacf610303920c3322be
ARG NODE_SHA256_ARM64=71e427e28b78846f201d4d5ecc30cb13d1508ca099ef3871889a1256c7d6f67e

# 先用 HTTP 华为源安装 ca-certificates，再切换为 HTTPS。
RUN sed -i 's|http://archive.ubuntu.com|http://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && sed -i 's|http://security.ubuntu.com|http://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && sed -i 's|http://repo.huaweicloud.com|https://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && rm -rf /var/lib/apt/lists/*

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git \
        jq \
        openssh-client \
        python3 \
        ripgrep \
        xz-utils \
    && arch="$(dpkg --print-architecture)" \
    && case "$arch" in \
        amd64) gh_arch=amd64; gh_sha256="${GH_SHA256_AMD64}" ;; \
        arm64) gh_arch=arm64; gh_sha256="${GH_SHA256_ARM64}" ;; \
        armhf) gh_arch=armv6; gh_sha256="${GH_SHA256_ARMV6}" ;; \
        *) echo "unsupported gh architecture: $arch" >&2; exit 1 ;; \
    esac \
    && curl -fL --retry 5 --retry-delay 2 --connect-timeout 20 --max-time 180 \
        "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_${gh_arch}.deb" \
        -o /tmp/gh.deb \
    && echo "${gh_sha256}  /tmp/gh.deb" | sha256sum -c - \
    && apt-get install -y --no-install-recommends /tmp/gh.deb \
    && gh --version \
    && rm -f /tmp/gh.deb \
    && rm -rf /var/lib/apt/lists/*

# stdio MCP 统一在 sidecar 内启动；固定 Node 版本并校验归档，
# 确保内置与自定义的 npx MCP 在不同 agent 镜像下行为一致。
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

RUN mkdir -p /workspace
WORKDIR /workspace
