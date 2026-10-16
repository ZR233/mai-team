#!/usr/bin/env python3
"""隔离验收用的项目夹具：本地 Git Smart HTTP + GitHub API（缺省 127.0.0.1:18082）。

只服务本次隔离实例：仓库、commit、PR 与 API 响应全部在 <root>/fixture 下本地生成，
全部使用假 token，绝不访问真实 GitHub、也绝不向外部发消息。

它提供三样东西：
1. 一个真实 bare 仓库 `<root>/fixture/git/reference-main.git`，含默认分支 `main`（BASE）与
   `refs/pull/1/head`（PR head），两个 commit 都由本地 `git` 真实创建；
2. Git Smart HTTP（upload-pack v1/v2），供 `url.<base>.insteadOf` 重写后的 `git fetch/clone` 使用；
3. GitHub REST 子集（PR/评审/提交/check-runs/status），供 Runtime 的项目审查只读解析使用，
   以及本地假的评审提交端点（只写本目录日志，不出网）。

启动后会写出 `<root>/fixture/github-fixture.json` 清单（BASE/HEAD SHA、仓库全名、安装标识、
BASE/HEAD 标记文件内容），驱动只读该清单，不猜 SHA。

用法（root 在验收机上执行）：

    python3 scripts/references/project-fixture.py --root <验收根> [--port 18082]
"""

import argparse
import json
import os
import pathlib
import shutil
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlparse, parse_qs

# 与驱动 `runtime_config::ACCEPTANCE_ROOT` 一致；只为缺省值，driver 会显式传 --root。
DEFAULT_ROOT = '/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources'

# 夹具仓库身份（项目仓库全名固定）。
OWNER = 'mai-reference-fixture'
NAME = 'reference-main'
REPO_FULL_NAME = f'{OWNER}/{NAME}'
DEFAULT_BRANCH = 'main'
PR_NUMBER = 1

# 假 token / 假安装：只用于本地 fixture。
INSTALLATION_ID = 4242
INSTALLATION_ACCOUNT = 'mai-reference-fixture'
APP_ID = 'fixture-app-1'
APP_BOT_LOGIN = 'mai-reference-fixture[bot]'
APP_BOT_USER_ID = 900001
FAKE_TOKEN = 'fixture-installation-token-4242'

# 两个 commit 的内容标记：驱动用真实工具读取它们来证明 /project/repo 冻结在 BASE。
BASE_LABEL_PATH = '.mai-fixture/base-label.txt'
HEAD_LABEL_PATH = '.mai-fixture/head-label.txt'
BASE_LABEL = 'reference-main-base'
HEAD_LABEL = 'reference-main-head'

# 固定提交时间，让两个 commit 的 SHA 跨运行稳定，便于对比历史证据。
FIXED_DATE = '2026-10-15T00:00:00+00:00'

BASE_FILES = {
    'README.md': '# reference-main\n\nIsolated acceptance fixture repository.\n',
    'AGENTS.md': 'Review constraint: documentation and code stay in Chinese.\n',
    'docs/constraints.md': 'Default branch constraint: no force pushes.\n',
    'src/fixture.c': 'int fixture_main(void) {\n    return 0;\n}\n',
    BASE_LABEL_PATH: BASE_LABEL + '\n',
}

HEAD_FILES = {
    'src/fixture.c': 'int fixture_main(void) {\n    return 1;\n}\n',
    'src/feature.c': 'int fixture_feature(void) {\n    return 42;\n}\n',
    HEAD_LABEL_PATH: HEAD_LABEL + '\n',
}


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', default=DEFAULT_ROOT)
    parser.add_argument('--host', default='127.0.0.1')
    parser.add_argument('--port', type=int, default=18082)
    return parser.parse_args()


def git(args, cwd=None, env=None, stdin=None):
    result = subprocess.run(
        ['git', *args],
        cwd=cwd,
        env=env,
        input=stdin,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(
            'git %s 失败：%s' % (' '.join(args), result.stderr.decode('utf-8', 'replace'))
        )
    return result.stdout.decode('utf-8', 'replace').strip()


def commit_env():
    env = os.environ.copy()
    env.update(
        {
            'GIT_AUTHOR_DATE': FIXED_DATE,
            'GIT_COMMITTER_DATE': FIXED_DATE,
            'GIT_AUTHOR_NAME': 'mai-fixture',
            'GIT_AUTHOR_EMAIL': 'fixture@mai-reference.invalid',
            'GIT_COMMITTER_NAME': 'mai-fixture',
            'GIT_COMMITTER_EMAIL': 'fixture@mai-reference.invalid',
        }
    )
    return env


def write_files(root, files):
    for relative, content in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding='utf-8')


def build_repository(fixture_dir):
    """重建 bare 仓库与两个 commit；返回 (bare 路径, base_sha, head_sha)。"""
    bare = fixture_dir / 'git' / f'{NAME}.git'
    scratch = fixture_dir / 'git-scratch'
    if bare.exists():
        shutil.rmtree(bare)
    if scratch.exists():
        shutil.rmtree(scratch)
    bare.parent.mkdir(parents=True, exist_ok=True)
    scratch.mkdir(parents=True, exist_ok=True)
    git(['init', '--bare', '--initial-branch', DEFAULT_BRANCH, str(bare)])

    env = commit_env()
    git(['init', '--initial-branch', DEFAULT_BRANCH, '.'], cwd=scratch)
    write_files(scratch, BASE_FILES)
    git(['add', '--all'], cwd=scratch)
    git(['commit', '--message', 'fixture base commit'], cwd=scratch, env=env)
    base_sha = git(['rev-parse', 'HEAD'], cwd=scratch)

    write_files(scratch, HEAD_FILES)
    (scratch / BASE_LABEL_PATH).write_text(BASE_LABEL + '\n', encoding='utf-8')
    (scratch / '.mai-fixture' / 'base-sha.txt').write_text(base_sha + '\n', encoding='utf-8')
    git(['add', '--all'], cwd=scratch)
    git(['commit', '--message', 'fixture pull request head commit'], cwd=scratch, env=env)
    head_sha = git(['rev-parse', 'HEAD'], cwd=scratch)

    git(['push', str(bare), f'{DEFAULT_BRANCH}:refs/heads/{DEFAULT_BRANCH}'], cwd=scratch, env=env)
    git(['push', str(bare), f'HEAD:refs/pull/{PR_NUMBER}/head'], cwd=scratch, env=env)
    git(['--git-dir', str(bare), 'symbolic-ref', 'HEAD', f'refs/heads/{DEFAULT_BRANCH}'])
    return bare, base_sha, head_sha


def pkt_line(payload):
    if isinstance(payload, str):
        payload = payload.encode('utf-8')
    return ('%04x' % (len(payload) + 4)).encode('ascii') + payload


class Fixture:
    """进程内共享的夹具状态：仓库路径、SHA、评审提交日志。"""

    def __init__(self, root, bare, base_sha, head_sha):
        self.root = root
        self.bare = bare
        self.base_sha = base_sha
        self.head_sha = head_sha
        self.lock = threading.Lock()
        self.reviews = []
        self.requests_dir = root / 'fixture' / 'requests'
        self.requests_dir.mkdir(parents=True, exist_ok=True)

    def repository_json(self):
        return {
            'id': 7001,
            'name': NAME,
            'full_name': REPO_FULL_NAME,
            'private': False,
            'owner': {'login': OWNER, 'type': 'Organization'},
            'clone_url': f'https://github.com/{REPO_FULL_NAME}.git',
            'html_url': f'https://github.com/{REPO_FULL_NAME}',
            'default_branch': DEFAULT_BRANCH,
        }

    def pull_request_json(self):
        return {
            'id': 8101,
            'number': PR_NUMBER,
            'state': 'open',
            'draft': False,
            'user': {'login': 'fixture-contributor', 'id': 900100},
            'head': {'sha': self.head_sha, 'ref': 'fixture-feature'},
            'base': {'sha': self.base_sha, 'ref': DEFAULT_BRANCH},
            'html_url': f'https://github.com/{REPO_FULL_NAME}/pull/{PR_NUMBER}',
            'title': 'fixture pull request',
            'merged': False,
        }

    def record_request(self, label, payload):
        with self.lock:
            serial = len(list(self.requests_dir.glob('request-*.json'))) + 1
            path = self.requests_dir / ('request-%05d.json' % serial)
            path.write_text(
                json.dumps({'label': label, 'payload': payload}, ensure_ascii=False, indent=2),
                encoding='utf-8',
            )


def api_response(fixture, method, path, body):
    """本地 GitHub REST 子集；返回 (status, payload)。"""
    segments = [unquote(segment) for segment in path.strip('/').split('/') if segment]
    if path == '/fixture/refs':
        return 200, {
            'repository_full_name': REPO_FULL_NAME,
            'base_sha': fixture.base_sha,
            'head_sha': fixture.head_sha,
            'pull_request': PR_NUMBER,
        }
    if path == '/user':
        return 200, {'login': APP_BOT_LOGIN, 'id': APP_BOT_USER_ID, 'type': 'Bot'}
    if len(segments) < 3 or segments[0] != 'repos':
        return 404, {'message': 'Not Found'}
    if f'{segments[1]}/{segments[2]}' != REPO_FULL_NAME:
        return 404, {'message': 'Not Found'}
    if len(segments) == 3:
        return 200, fixture.repository_json()
    kind = segments[3]
    rest = segments[4:]
    if kind == 'pulls':
        if not rest:
            return 200, [fixture.pull_request_json()]
        if rest[0] != str(PR_NUMBER):
            return 404, {'message': 'Not Found'}
        if len(rest) == 1:
            return 200, fixture.pull_request_json()
        if rest[1:] != ['reviews']:
            return 404, {'message': 'Not Found'}
        if method == 'POST':
            fixture.record_request('pull_request_review', body)
            with fixture.lock:
                review = {
                    'id': 9100 + len(fixture.reviews) + 1,
                    'state': (body or {}).get('event', 'COMMENTED'),
                    'body': (body or {}).get('body', ''),
                    'commit_id': (body or {}).get('commit_id', fixture.head_sha),
                    'submitted_at': '2026-10-15T00:00:00Z',
                    'user': {'login': APP_BOT_LOGIN, 'id': APP_BOT_USER_ID},
                }
                fixture.reviews.append(review)
            return 201, review
        with fixture.lock:
            return 200, list(fixture.reviews)
    if kind == 'commits' and rest:
        fixture.record_request('commits', {'path': path, 'sha': rest[0]})
        if len(rest) == 1:
            return 200, {
                'sha': rest[0],
                'commit': {
                    'message': 'fixture commit',
                    'committer': {'name': 'mai-fixture', 'date': FIXED_DATE},
                },
            }
        if rest[1] == 'check-runs':
            return 200, {'total_count': 0, 'check_runs': []}
        if rest[1] == 'status':
            return 200, {'state': 'success', 'total_count': 0, 'statuses': []}
    return 404, {'message': 'Not Found'}


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    fixture = None

    def log_message(self, *_):
        pass

    # ---- 基础收发 ----

    def send_payload(self, status, payload, content_type):
        self.send_response(status)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def send_json(self, status, value):
        self.send_payload(status, json.dumps(value).encode('utf-8'), 'application/json')

    def read_body(self):
        if self.headers.get('Transfer-Encoding', '').lower() == 'chunked':
            chunks = []
            while True:
                line = self.rfile.readline().strip()
                if not line:
                    continue
                size = int(line.split(b';')[0], 16)
                if size == 0:
                    self.rfile.readline()
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.read(2)
            return b''.join(chunks)
        length = int(self.headers.get('Content-Length') or 0)
        return self.rfile.read(length) if length else b''

    def git_env(self):
        env = os.environ.copy()
        protocol = self.headers.get('Git-Protocol')
        if protocol:
            env['GIT_PROTOCOL'] = protocol
        return env

    # ---- Git Smart HTTP ----

    def git_kind(self, path):
        candidate = path.lstrip('/')
        if candidate.startswith(OWNER + '/'):
            candidate = candidate[len(OWNER) + 1:]
        if candidate.endswith('/info/refs'):
            name = candidate[: -len('/info/refs')]
            return 'refs' if name in (NAME, NAME + '.git') else None
        if candidate.endswith('/git-upload-pack'):
            name = candidate[: -len('/git-upload-pack')]
            return 'upload' if name in (NAME, NAME + '.git') else None
        return None

    def serve_git(self, kind):
        bare = str(self.fixture.bare)
        if kind == 'refs':
            result = subprocess.run(
                ['git', 'upload-pack', '--stateless-rpc', '--advertise-refs', bare],
                env=self.git_env(),
                capture_output=True,
                check=False,
            )
            body = pkt_line('# service=git-upload-pack\n') + b'0000' + result.stdout
            self.send_payload(200, body, 'application/x-git-upload-pack-advertisement')
            return
        request_body = self.read_body()
        result = subprocess.run(
            ['git', 'upload-pack', '--stateless-rpc', bare],
            env=self.git_env(),
            input=request_body,
            capture_output=True,
            check=False,
        )
        self.send_payload(200, result.stdout, 'application/x-git-upload-pack-result')

    def do_GET(self):
        parsed = urlparse(self.path)
        kind = self.git_kind(parsed.path)
        if kind is not None and parse_qs(parsed.query).get('service') == ['git-upload-pack']:
            self.serve_git(kind)
            return
        status, payload = api_response(self.fixture, 'GET', parsed.path, None)
        self.send_json(status, payload)

    def do_POST(self):
        parsed = urlparse(self.path)
        if self.git_kind(parsed.path) == 'upload':
            self.serve_git('upload')
            return
        raw = self.read_body()
        body = None
        if raw:
            try:
                body = json.loads(raw)
            except ValueError:
                self.send_json(400, {'message': 'invalid JSON body'})
                return
        status, payload = api_response(self.fixture, 'POST', parsed.path, body)
        self.send_json(status, payload)


def main():
    args = parse_args()
    root = pathlib.Path(args.root).resolve()
    allowed = pathlib.Path(DEFAULT_ROOT)
    if root != allowed and allowed not in root.parents:
        raise SystemExit('夹具根必须是本次隔离实例：%s' % allowed)
    fixture_dir = root / 'fixture'
    fixture_dir.mkdir(parents=True, exist_ok=True)
    bare, base_sha, head_sha = build_repository(fixture_dir)
    if base_sha == head_sha:
        raise SystemExit('BASE 与 PR head 不能是同一个 commit')
    fixture = Fixture(root, bare, base_sha, head_sha)
    manifest = {
        'fixture': 'mai-references-project-fixture',
        'created_at': time.strftime('%Y-%m-%dT%H:%M:%S%z'),
        'github_api_base_url': 'http://%s:%d' % (args.host, args.port),
        'git_service_base_url': 'http://%s:%d' % (args.host, args.port),
        'repository_full_name': REPO_FULL_NAME,
        'owner': OWNER,
        'name': NAME,
        'default_branch': DEFAULT_BRANCH,
        'pull_request': PR_NUMBER,
        'base_sha': base_sha,
        'head_sha': head_sha,
        'installation_id': INSTALLATION_ID,
        'installation_account': INSTALLATION_ACCOUNT,
        'app_id': APP_ID,
        'app_bot_login': APP_BOT_LOGIN,
        'app_bot_user_id': APP_BOT_USER_ID,
        'fake_token': FAKE_TOKEN,
        'base_label_path': BASE_LABEL_PATH,
        'base_label': BASE_LABEL,
        'head_label_path': HEAD_LABEL_PATH,
        'head_label': HEAD_LABEL,
        'bare_repository': str(bare),
    }
    manifest_path = fixture_dir / 'github-fixture.json'
    manifest_path.write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding='utf-8')
    Handler.fixture = fixture
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print('project fixture listening on http://%s:%d' % (args.host, args.port), flush=True)
    print('manifest: %s' % manifest_path, flush=True)
    print('base_sha=%s head_sha=%s' % (base_sha, head_sha), flush=True)
    server.serve_forever()


if __name__ == '__main__':
    main()
