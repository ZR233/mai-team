# Mai Relay 安装说明

`install-mai-relay-ubuntu-24.04.sh` 用于在 Ubuntu 24.04 x86_64 主机上安装 `mai-relay`，并注册为 systemd 服务。

`update-mai-relay-ubuntu-24.04.sh` 用于更新已安装的 `mai-relay`。它和安装脚本使用同一组参数，默认保留已有 token、public URL、bind addr 和 sqlite 路径，替换二进制、刷新 systemd service 文件并重启服务。

## 一键安装

在 relay 服务器上执行：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/install-mai-relay-ubuntu-24.04.sh | sudo bash -s -- --public-url "http://YOUR_RELAY_HOST:8090"
```

如果直接使用最新 release，脚本会下载：

```text
https://github.com/ZR233/mai-team/releases/latest/download/mai-relay-x86_64-unknown-linux-gnu.tar.gz
```

## 指定版本安装

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/install-mai-relay-ubuntu-24.04.sh | sudo bash -s -- --version vX.Y.Z --public-url "http://YOUR_RELAY_HOST:8090"
```

## 一键更新

在已安装 relay 的服务器上执行：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/update-mai-relay-ubuntu-24.04.sh | sudo bash
```

指定版本更新：

```bash
curl -fsSL https://raw.githubusercontent.com/ZR233/mai-team/main/scripts/update-mai-relay-ubuntu-24.04.sh | sudo bash -s -- --version vX.Y.Z
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
--version vX.Y.Z      安装指定 release 版本；默认 latest
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
