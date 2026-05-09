FROM ubuntu:24.04

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
    unzip \
    openssh-client \
    vim-tiny \
    tree \
    less \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

# 创建工作目录
RUN mkdir -p /workspace
WORKDIR /workspace
