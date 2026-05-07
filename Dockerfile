FROM ubuntu:24.04

# 替换为华为镜像源
RUN sed -i 's|http://archive.ubuntu.com|https://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && sed -i 's|http://security.ubuntu.com|https://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources

# 安装基础工具
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    curl \
    wget \
    ripgrep \
    fd-find \
    jq \
    unzip \
    ca-certificates \
    openssh-client \
    vim-tiny \
    tree \
    less \
    && rm -rf /var/lib/apt/lists/*

# 创建工作目录
RUN mkdir -p /workspace
WORKDIR /workspace
