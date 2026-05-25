# Mai Relay 安装说明

`install-mai-relay-ubuntu-24.04.sh` 用于在 Ubuntu 22.04 或 24.04 x86_64 主机上安装 `mai-relay`，并注册为 systemd 服务。

`update-mai-relay-ubuntu-24.04.sh` 用于更新已安装的 `mai-relay`。它和安装脚本使用同一组参数，默认保留已有 token、public URL、bind addr 和 sqlite 路径，替换二进制、刷新 systemd service 文件并重启服务。

Relay release 由 release-plz 管理，只发布 GitHub release，不发布 crates.io。Release tag 使用 release-plz 多包 workspace 的包名前缀形态，固定为 `mai-relay-vX.Y.Z`，脚本的 `--version` 参数也应传入对应 GitHub tag，例如 `--version mai-relay-v0.1.2`。

## 一键安装

在 relay 服务器上执行：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/install-mai-relay-ubuntu-24.04.sh | sudo bash -s -- --public-url "http://YOUR_RELAY_HOST:8090"
```

如果直接使用最新 release，脚本会先从 GitHub releases 列表中选择最新的 `mai-relay-vX.Y.Z` tag，再下载：

```text
https://github.com/ZR233/mai-team/releases/download/mai-relay-vX.Y.Z/mai-relay-x86_64-unknown-linux-gnu.tar.gz
```

release-plz 发布流程会为该 release 上传同名二进制包和 `.sha256` 校验文件。不要改动这两个 asset 名称，否则安装脚本和 relay 自更新程序都需要同步调整。

## 指定版本安装

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/install-mai-relay-ubuntu-24.04.sh | sudo bash -s -- --version mai-relay-vX.Y.Z --public-url "http://YOUR_RELAY_HOST:8090"
```

## 一键更新

在已安装 relay 的服务器上执行：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/update-mai-relay-ubuntu-24.04.sh | sudo bash
```

指定版本更新：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/update-mai-relay-ubuntu-24.04.sh | sudo bash -s -- --version mai-relay-vX.Y.Z
```

## 安装内容

脚本会安装或更新：

- `/opt/mai-relay/mai-relay`
- `/usr/local/bin/mai-relay`（指向 `/opt/mai-relay/mai-relay` 的兼容 symlink）
- `/etc/mai-relay/mai-relay.env`
- `/var/lib/mai-relay/mai-relay.sqlite3`
- `/etc/systemd/system/mai-relay.service`

`mai-relay` 服务会以 `mai-relay` 用户运行，并拥有 `/opt/mai-relay`。这样 Settings 页面触发自更新时，relay 可以下载最新 release、校验 sha256、替换自身二进制，然后退出并由 systemd `Restart=always` 自动拉起新版本，不需要 sudo/root helper。

安装完成后会执行：

```bash
systemctl enable --now mai-relay
```

并打印给 `mai-server` 连接使用的：

- Relay URL
- Node ID
- Relay token

## 参数

```text
--version mai-relay-vX.Y.Z
                     安装指定 release 版本；默认 latest
--public-url URL      设置 relay 对外访问地址；默认 http://127.0.0.1:8090
--rotate-token        重新生成 relay token；默认保留旧 token
--dry-run             只打印将要执行的安装动作，不写入系统
```

更新脚本也支持同样的参数。更新时如果不传 `--public-url`，会保留 `/etc/mai-relay/mai-relay.env` 里已有的 `MAI_RELAY_PUBLIC_URL`。

## 检查状态

```bash
systemctl is-enabled mai-relay
systemctl is-active mai-relay
journalctl -u mai-relay -n 100 --no-pager
```

## 更新

推荐优先在 `mai-server` Settings > GitHub App 的 Relay Update 面板中检查并更新 relay。命令行一键更新脚本仍可用于首次迁移旧安装、应用 systemd service 文件改动或手动恢复；默认会保留 `/etc/mai-relay/mai-relay.env` 里已有的 token 和 public URL。

需要强制轮换 token 时：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/update-mai-relay-ubuntu-24.04.sh | sudo bash -s -- --rotate-token
```

## mai-server 配置

在 `mai-server` 的 GitHub App 设置页中填写：

- Relay URL：安装脚本输出的 Relay URL
- Relay token：安装脚本输出的 Relay token
- Node ID：通常为 `mai-server`

保存后 `mai-server` 会立即应用 relay 连接设置。

# Mai Server 安装说明

`install-mai-server-ubuntu-24.04.sh` 用于在 Ubuntu 22.04 或 24.04 x86_64 主机上安装 `mai-server`，并注册为 systemd 服务。

`update-mai-server-ubuntu-24.04.sh` 用于更新已安装的 `mai-server`。更新脚本会替换二进制、刷新 systemd service 文件、执行 `systemctl daemon-reload`，并重启服务。

更新脚本会管理 `MAI_BIND_ADDR` 和 `RUST_LOG`，并保留 `/etc/mai-server/mai-server.env` 中其他自定义环境变量。

`mai-server` release 由 GitHub release 提供。Release tag 使用包名前缀形态，固定为 `mai-server-vX.Y.Z`，脚本的 `--version` 参数也应传入对应 GitHub tag，例如 `--version mai-server-v0.1.2`。release asset 名固定为 `mai-server-x86_64-unknown-linux-gnu.tar.gz`。

## Mai Server 一键安装

在 `mai-server` 主机上执行：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/install-mai-server-ubuntu-24.04.sh | sudo bash -s -- --bind-addr 0.0.0.0:8080
```

如果直接使用最新 release，脚本会先从 GitHub releases 列表中选择最新的 `mai-server-vX.Y.Z` tag，再下载：

```text
https://github.com/ZR233/mai-team/releases/download/mai-server-vX.Y.Z/mai-server-x86_64-unknown-linux-gnu.tar.gz
```

release 发布流程需要为该 release 上传同名二进制包和 `.sha256` 校验文件。不要改动 asset 名称，否则安装脚本和更新脚本都需要同步调整。

## Mai Server 指定版本安装

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/install-mai-server-ubuntu-24.04.sh | sudo bash -s -- --version mai-server-vX.Y.Z
```

## Mai Server 更新

在已安装 `mai-server` 的服务器上执行：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/update-mai-server-ubuntu-24.04.sh | sudo bash
```

指定版本更新：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/update-mai-server-ubuntu-24.04.sh | sudo bash -s -- --version mai-server-vX.Y.Z
```

## Mai Server 安装内容

脚本会安装或更新：

- `/opt/mai-server/mai-server`
- `/usr/local/bin/mai-server`（指向 `/opt/mai-server/mai-server` 的兼容 symlink）
- `/etc/mai-server/mai-server.env`
- `/var/lib/mai-server`
- `/etc/systemd/system/mai-server.service`

systemd service 会通过 `/etc/mai-server/mai-server.env` 读取环境变量，并使用固定数据目录启动：

```text
ExecStart=/opt/mai-server/mai-server --data-path /var/lib/mai-server
```

安装完成后会执行：

```bash
systemctl enable --now mai-server
```

## Mai Server 参数

```text
--version mai-server-vX.Y.Z
                     安装或更新指定 release 版本；默认 latest
--bind-addr HOST:PORT
                     设置服务监听地址；安装默认 0.0.0.0:8080，更新默认保留已有 MAI_BIND_ADDR
--dry-run             只打印将要执行的安装或更新动作，不写入系统
```

需要调整监听地址、模型 provider、默认 Docker 镜像或 relay 连接时，修改 `/etc/mai-server/mai-server.env` 后重启服务。常用环境变量包括：

```text
MAI_BIND_ADDR=0.0.0.0:8080
OPENAI_API_KEY=
OPENAI_BASE_URL=https://api.openai.com/v1
OPENAI_MODEL=gpt-5.5
MAI_AGENT_BASE_IMAGE=ghcr.io/zr233/mai-team-agent:latest
MAI_SIDECAR_IMAGE=ghcr.io/zr233/mai-team-sidecar:latest
```

## Docker 前置条件

安装脚本不安装 Docker。运行 `mai-server` 前需要主机上的 Docker daemon 可用，并且 `mai-server` 用户可以访问 Docker socket。

可用以下命令检查：

```bash
docker version
sudo -u mai-server docker version
```

如果 `mai-server` 用户还不能访问 Docker socket，请先按服务器安全策略配置用户组或 socket 权限，再启动服务。

## Mai Server 状态检查

```bash
systemctl is-enabled mai-server
systemctl is-active mai-server
journalctl -u mai-server -n 100 --no-pager
```

## Mai Server 健康检查

```bash
curl -fsS http://127.0.0.1:8080/health
```
