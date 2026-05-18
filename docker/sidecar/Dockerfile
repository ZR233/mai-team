FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ARG GH_VERSION=2.92.0
ARG GH_SHA256_AMD64=8f8212b1a9cec261a8839e0893168f50d3fc70f095da257feef4229234cefdf8
ARG GH_SHA256_ARM64=34d620b7c884774ed86236541535170889fda0b99aafbdab8b69c7d458b5ca6b
ARG GH_SHA256_ARMV6=acf889527e8351f056bcedf959d9a4c011d4788ed89eb0ba6ca46bdb137b471d

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

RUN mkdir -p /workspace
WORKDIR /workspace
