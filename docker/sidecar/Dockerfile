FROM ubuntu:24.04 AS github-mcp-build

RUN sed -i 's|http://archive.ubuntu.com|http://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && sed -i 's|http://security.ubuntu.com|http://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && sed -i 's|http://repo.huaweicloud.com|https://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends git golang-go \
    && rm -rf /var/lib/apt/lists/*
RUN GOPROXY=https://goproxy.cn,direct GOBIN=/out go install github.com/github/github-mcp-server/cmd/github-mcp-server@latest

FROM ubuntu:24.04

# 先用 HTTP 华为源安装 ca-certificates，再切换为 HTTPS
RUN sed -i 's|http://archive.ubuntu.com|http://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && sed -i 's|http://security.ubuntu.com|http://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && sed -i 's|http://repo.huaweicloud.com|https://repo.huaweicloud.com|g' /etc/apt/sources.list.d/ubuntu.sources \
    && rm -rf /var/lib/apt/lists/*

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    curl \
    jq \
    openssh-client \
    && rm -rf /var/lib/apt/lists/*

# Install official MCP helpers used by Mai's built-in Git/GitHub integrations.
ENV UV_INSTALL_DIR=/usr/local/bin
RUN curl -LsSf https://astral.sh/uv/install.sh | sh \
    && uvx --from mcp-server-git mcp-server-git --help >/dev/null
COPY --from=github-mcp-build /out/github-mcp-server /usr/local/bin/github-mcp-server

RUN mkdir -p /workspace
WORKDIR /workspace
