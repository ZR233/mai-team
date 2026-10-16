#!/usr/bin/env bash
# 仅在本机 Docker + 临时 fixture 上测试。不会读取/修改用户的参考源码。
set -Eeuo pipefail
image="${REF_TEST_IMAGE:?请通过 REF_TEST_IMAGE 指定已准备好的可信测试镜像，镜像需有 sh/coreutils}"
command -v docker >/dev/null
# 防止 Docker 自动拉取，避免测试隐含联网行为。
docker image inspect "$image" >/dev/null
# 与 Docker CLI 优先级保持一致：DOCKER_CONTEXT 覆盖 DOCKER_HOST。
if [[ -n "${DOCKER_CONTEXT:-}" ]]; then
  endpoint="$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}')"
elif [[ -n "${DOCKER_HOST:-}" ]]; then
  endpoint="$DOCKER_HOST"
else
  endpoint="$(docker context inspect --format '{{.Endpoints.docker.Host}}')"
fi
case "$endpoint" in unix://*) ;; *) echo '仅支持同机 Unix-socket Docker；拒绝远程 fixture 路径' >&2; exit 1;; esac
fixture="$(mktemp -d -t mai-ref-smoke.XXXXXXXX)"
label="mai.reference.smoke=$(basename "$fixture")"
containers=()
cleanup() {
  local id
  for id in "${containers[@]}"; do docker rm -f "$id" >/dev/null 2>&1 || true; done
  chmod -R u+w "$fixture" 2>/dev/null || true
  rm -rf -- "$fixture"
}
trap cleanup EXIT
printf 'shared-fixture-only\n' > "$fixture/original.txt"
printf 'remove-rename-fixture\n' > "$fixture/remove.txt"
# 特意允许宿主写入，保证测的是 mount RO 而非 chmod 假保护。
chmod 0777 "$fixture"
chmod 0666 "$fixture/"*.txt
before="$(sha256sum "$fixture/"*.txt)"
for name in first second; do
  id="$(docker create --label "$label" --network none --user root \
    --mount "type=bind,src=$fixture,dst=/references/test,readonly,bind-recursive=readonly,bind-propagation=rprivate" \
    "$image" sh -c 'sleep 300')"
  containers+=("$id")
  docker start "$id" >/dev/null
  test "$(docker exec "$id" cat /references/test/original.txt)" = 'shared-fixture-only'
  test "$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/references/test"}}{{.RW}}{{end}}{{end}}' "$id")" = 'false'
  test "$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/references/test"}}{{.Source}}{{end}}{{end}}' "$id")" = "$fixture"
done
# 验证当前 sidecar 采用的 volumes-from 不会把 RO 变成 RW。
sidecar="$(docker create --label "$label" --network none --user root \
  --volumes-from "${containers[0]}" "$image" sh -c 'sleep 300')"
containers+=("$sidecar")
docker start "$sidecar" >/dev/null
for id in "${containers[@]}"; do
  test "$(docker inspect --format '{{range .Mounts}}{{if eq .Destination "/references/test"}}{{.RW}}{{end}}{{end}}' "$id")" = 'false'
  docker exec "$id" sh -eu -c '
    expect_denied() {
      if "$@" >/dev/null 2>&1; then
        echo "本应失败的操作成功：$*" >&2
        exit 1
      fi
    }
    expect_denied sh -c "printf bad >> /references/test/original.txt"
    expect_denied touch /references/test/new.txt
    expect_denied rm /references/test/remove.txt
    expect_denied mv /references/test/remove.txt /references/test/moved.txt
    expect_denied chmod 0777 /references/test/original.txt
    expect_denied ln /references/test/original.txt /references/test/hardlink.txt
  '
done
after="$(sha256sum "$fixture/"*.txt)"
test "$before" = "$after"
test ! -e "$fixture/new.txt"
test ! -e "$fixture/moved.txt"
test ! -e "$fixture/hardlink.txt"
echo 'PASS：两个 Agent 样式容器及一个 volumes-from sidecar 共享同一 fixture，均 RO；宿主摘要未变化。'
echo '本 smoke 不替代子挂载、安全门禁、容器换代、PL 工具、恢复与 Web UI 的完整验收。'
