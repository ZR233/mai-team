#!/usr/bin/env python3
"""隔离验收的确定性 Responses 服务；工具仍由真实 Runtime 注册和执行。"""
import argparse
import http.server
import json
import pathlib
import threading
import time
import uuid

parser = argparse.ArgumentParser()
parser.add_argument('--evidence-dir', required=True)
args = parser.parse_args()
root = pathlib.Path(args.evidence_dir).resolve()
allowed = pathlib.Path('/mnt/docker_nvme/mai-references-acceptance/20261015-shared-sources')
if allowed not in root.parents:
    raise SystemExit('证据目录必须属于本次隔离实例')
root.mkdir(parents=True, exist_ok=True)
lock = threading.Lock()
issued = set()

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        body = json.dumps({'ok': True, 'issued': sorted(issued)}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        with lock:
            serial = len(list(root.glob('request-*.json'))) + 1
            (root / ('request-%05d.json' % serial)).write_text(json.dumps(request, ensure_ascii=False, indent=2))
        inputs = request.get('input', [])
        texts = []
        for item in inputs if isinstance(inputs, list) else []:
            if item.get('role') == 'user':
                content = item.get('content', [])
                texts += [content] if isinstance(content, str) else [part.get('text', '') for part in content]
        probe = None
        for text in reversed(texts):
            marker = 'MAI_REFERENCE_PROBE '
            if marker in text:
                probe = json.loads(text.split(marker, 1)[1])
                break
        # 自动 Reviewer 初始消息来自产品。留出发现并提交 probe 的窗口，不伪造其工具结果。
        if probe is None and any(text.startswith('Run one automatic pull request review for project `') for text in texts[-1:]):
            time.sleep(30)
        response_id = 'resp_' + uuid.uuid4().hex
        model = request.get('model', 'gpt-5.5')
        events = [{'type': 'response.created', 'response': {'id': response_id, 'model': model}}]
        output = []
        with lock:
            first = probe is not None and probe['probe_id'] not in issued
            if first:
                issued.add(probe['probe_id'])
        if first:
            time.sleep(probe.get('delay_seconds', 0))
            for index, call in enumerate(probe.get('tools', [])):
                call_id = 'call_' + probe['probe_id'] + '_' + str(index)
                item = {'id': 'fc_' + call_id, 'call_id': call_id, 'type': 'function_call', 'name': call['name'], 'arguments': json.dumps(call['arguments'], ensure_ascii=False), 'status': 'completed'}
                output.append(item)
                events.append({'type': 'response.output_item.done', 'output_index': index, 'item': item})
        else:
            text = 'MAI_REFERENCE_PROBE_DONE ' + (probe['probe_id'] if probe else 'health')
            events += [
                {'type': 'response.output_text.delta', 'output_index': 0, 'item_id': 'message-1', 'content_index': 0, 'delta': text},
                {'type': 'response.output_item.done', 'output_index': 0, 'item': {'id': 'message-1', 'type': 'message', 'role': 'assistant', 'content': [{'type': 'output_text', 'text': text}]}},
            ]
        events.append({'type': 'response.completed', 'response': {'id': response_id, 'model': model, 'output': output, 'usage': {'input_tokens': 11, 'output_tokens': 3}}})
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Connection', 'close')
        self.end_headers()
        try:
            for event in events:
                self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

http.server.ThreadingHTTPServer(('127.0.0.1', 18081), Handler).serve_forever()
