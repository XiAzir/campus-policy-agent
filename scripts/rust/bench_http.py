#!/usr/bin/env python3
"""Linux full-process HTTP benchmark with synthetic data and a loopback-only model.

No production directory or credentials are used. The server gets a fresh temporary
DATA_DIR for every run. RSS includes its Tokio/SQLite/HTTP/agent workers, not this
client, its mock upstream, or the kernel page cache. This is not a real-API or
cgroup-OOM qualification. Generate D1 first with the corrected gen_d1.py.
"""
from __future__ import annotations

import argparse
import base64
import concurrent.futures
import hashlib
import hmac
import http.server
import json
import math
import os
from pathlib import Path
import platform
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


class MockModel(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if self.path.endswith('/embeddings'):
            obj = {'data': [{'embedding': [1.0] + [0.0] * 1023}],
                   'usage': {'total_tokens': 5}}
            mime = 'application/json'
            raw = json.dumps(obj).encode()
        else:
            has_tool = any('functionResponse' in part
                           for msg in body.get('contents', [])
                           for part in msg.get('parts', []))
            parts = ([{'text': '以下为合成资料检索结果，仅用于基准测试。[[EV1]]'}]
                     if has_tool else [{'functionCall': {'name': 'policy_search',
                         'args': {'query': '学生 处分 申诉'}, 'id': 'bench-search'}}])
            time.sleep(0.01)  # Deterministic mock latency, not a real provider.
            obj = {'candidates': [{'content': {'role': 'model', 'parts': parts}}],
                   'usageMetadata': {'promptTokenCount': 100, 'candidatesTokenCount': 20}}
            mime = 'text/event-stream'
            raw = ('data: ' + json.dumps(obj, ensure_ascii=False) + '\n\n').encode()
        self.send_response(200)
        self.send_header('Content-Type', mime)
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


def request(base, path, token=None, obj=None, raw=None, mime=None):
    headers = {}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    if obj is not None:
        raw = json.dumps(obj, ensure_ascii=False).encode()
        mime = 'application/json'
    if mime:
        headers['Content-Type'] = mime
    req = urllib.request.Request(base + path, data=raw, headers=headers)
    try:
        return urllib.request.urlopen(req, timeout=180)
    except urllib.error.HTTPError as exc:
        detail = exc.read(4096).decode(errors='replace')
        raise RuntimeError(f'{path}: HTTP {exc.code}: {detail}') from exc


def call(base, path, **kwargs):
    with request(base, path, **kwargs) as response:
        return json.load(response)


def temporary_token(secret, role, client):
    # Only the brand-new benchmark database is read; never a user's database.
    payload = {'r': role, 'c': client, 'e': int(time.time()) + 3600}
    raw = base64.urlsafe_b64encode(json.dumps(payload, separators=(',', ':')).encode()).rstrip(b'=')
    sig = hmac.new(bytes.fromhex(secret), raw, hashlib.sha256).hexdigest()[:32]
    return raw.decode() + '.' + sig


def proc_stats(pid):
    fields = {}
    for line in Path(f'/proc/{pid}/status').read_text().splitlines():
        key, _, value = line.partition(':')
        if key in ('VmRSS', 'VmHWM', 'Threads'):
            fields[key] = int(value.split()[0])
    stat = Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()
    fields['cpu_s'] = (int(stat[11]) + int(stat[12])) / os.sysconf('SC_CLK_TCK')
    fields['fds'] = len(list(Path(f'/proc/{pid}/fd').iterdir()))
    return fields


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--dataset', required=True, type=Path, help='Directory with D1 ZIPs')
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--label', required=True)
    parser.add_argument('--work-root', type=Path, default=None)
    parser.add_argument('--turns', type=int, default=40)
    args = parser.parse_args()
    if not 1 <= args.turns <= 200:
        parser.error('--turns must be 1..200')
    packages = sorted(args.dataset.glob('d1-pkg-*.zip'))
    if len(packages) != 5:
        parser.error('D1 must contain exactly five generated packages')
    mock = http.server.ThreadingHTTPServer(('127.0.0.1', 0), MockModel)
    threading.Thread(target=mock.serve_forever, daemon=True).start()
    process = None
    finished = threading.Event()
    phase = ['startup']
    peaks = {}
    result = {'label': args.label, 'platform': platform.platform(),
              'binary_sha256': hashlib.sha256(args.binary.read_bytes()).hexdigest(),
              'packages': [{'name': p.name, 'sha256': hashlib.sha256(p.read_bytes()).hexdigest()}
                           for p in packages],
              'sampling_interval_ms': 10, 'mock_latency_ms_per_model_call': 10,
              'cpu_affinity': None, 'memory_limit': 'not imposed by this script',
              'filesystem': str(args.work_root or tempfile.gettempdir())}
    try:
        with tempfile.TemporaryDirectory(prefix='campus-http-bench-', dir=args.work_root) as tmp:
            root = Path(tmp)
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0))
                port = sock.getsockname()[1]
            env = os.environ.copy()
            # Clear all optional knobs so ambient project credentials/tuning cannot affect the run.
            for name in list(env):
                if name.startswith(('CHAT_', 'GEMINI_', 'SILICONFLOW_', 'MAX_PACKAGE_',
                                    'MAX_UPLOAD_', 'TOKEN_TTL_', 'INITIAL_ADMIN_', 'PREPROCESSING_')):
                    env.pop(name)
            upstream = f'http://127.0.0.1:{mock.server_port}'
            env.update(DATA_DIR=str(root/'data'), FRONTEND_DIST=str(root/'no-ui'),
                       HOST='127.0.0.1', PORT=str(port), EMBED_DIMS='1024',
                       GEMINI_BASE_URL=upstream, GEMINI_MODEL='bench-model', GEMINI_API_KEY='mock-only',
                       SILICONFLOW_BASE_URL=upstream, SILICONFLOW_EMBED_MODEL='test-embed',
                       SILICONFLOW_API_KEY='mock-only', INITIAL_ADMIN_PASSWORD='benchmark-only')
            log = open(root/'server.log', 'wb')
            core = min(os.sched_getaffinity(0))
            # taskset runs before Rust starts any worker, so every thread inherits one CPU.
            process = subprocess.Popen(['taskset', '-c', str(core), str(args.binary.resolve())],
                                       env=env, stdout=log, stderr=log)
            result['cpu_affinity'] = [core]
            base = f'http://127.0.0.1:{port}'
            for _ in range(600):
                if process.poll() is not None:
                    raise RuntimeError((root/'server.log').read_text())
                try:
                    call(base, '/api/auth/state')
                    break
                except (OSError, RuntimeError):
                    time.sleep(0.05)
            else:
                raise RuntimeError('Server did not become ready')

            def sample():
                while not finished.wait(0.01):
                    try:
                        stat = proc_stats(process.pid)
                        name = phase[0]
                        peaks[name] = max(peaks.get(name, 0), stat['VmRSS'])
                    except (OSError, ValueError, KeyError):
                        pass
            threading.Thread(target=sample, daemon=True).start()
            result['idle'] = proc_stats(process.pid)
            # Exercise the real password login once, then use fresh synthetic client IDs
            # to avoid measuring the intentional 20-request per-user rate limit.
            admin = call(base, '/api/auth/admin/login', obj={'password': 'benchmark-only'})['admin_token']
            with sqlite3.connect(root/'data/campus.db') as db:
                secret = db.execute("SELECT value FROM settings WHERE key='token_secret'").fetchone()[0]
            user = lambda i: temporary_token(secret, 'user', f'benchmark-client-{i}')
            phase[0] = 'import_publish'
            t0 = time.perf_counter()
            for package in packages:
                boundary = 'campus-benchmark-boundary'
                raw = (f'--{boundary}\r\nContent-Disposition: form-data; name="file"; '
                       f'filename="{package.name}"\r\nContent-Type: application/zip\r\n\r\n').encode()
                raw += package.read_bytes() + f'\r\n--{boundary}--\r\n'.encode()
                imported = call(base, '/api/admin/packages', token=admin, raw=raw,
                                mime=f'multipart/form-data; boundary={boundary}')
                call(base, f"/api/admin/packages/{imported['id']}/publish", token=admin,
                     obj={'replacements': {}})
            result['import_publish_s'] = time.perf_counter() - t0
            result['after_import'] = proc_stats(process.pid)
            catalog = call(base, '/api/catalog', token=user(0))['documents']
            if len(catalog) != 100:
                raise AssertionError(f'Expected 100 published docs, got {len(catalog)}')
            with sqlite3.connect(root/'data/campus.db') as db:
                result['chunks'] = db.execute('SELECT count(*) FROM chunks').fetchone()[0]
            if result['chunks'] != 10000:
                raise AssertionError('Expected 10000 chunks')
            result['documents'] = len(catalog)

            def chat(i):
                started = time.perf_counter()
                with request(base, '/api/chat', token=user(i), obj={
                        'question': '学生处分如何申诉？', 'messages': [], 'profile': {},
                        'scope': {'mode': 'auto', 'year_mode': 'current'}}) as response:
                    events = [json.loads(line[5:]) for line in response.read().splitlines()
                              if line.startswith(b'data:')]
                errors = [e for e in events if e.get('event') == 'error']
                done = [e for e in events if e.get('event') == 'done']
                metrics = [e for e in events if e.get('event') == 'metrics']
                citations = [e for e in events if e.get('event') == 'citations']
                if errors or not done or not metrics or not citations or not citations[0].get('citations'):
                    raise AssertionError(f'Incomplete grounded chat: {events}')
                return {'http_ms': (time.perf_counter()-started)*1000,
                        'retrieval_ms': metrics[-1]['retrieval_s']*1000,
                        'model_calls': metrics[-1]['model_calls'],
                        'embedding_calls': metrics[-1]['embedding_calls']}

            phase[0] = 'warmup'
            for i in range(5):
                chat(100+i)
            result['warm'] = proc_stats(process.pid)
            phase[0] = 'sequential_chat'
            cpu0 = proc_stats(process.pid)['cpu_s']
            turns = [chat(1000+i) for i in range(args.turns)]
            result['chat_cpu_s'] = proc_stats(process.pid)['cpu_s'] - cpu0
            result['sequential_turns'] = turns
            for field in ['http_ms', 'retrieval_ms']:
                for name, quantile in [('p50', .5), ('p95', .95)]:
                    result[f'{field}_{name}'] = percentile([t[field] for t in turns], quantile)
            phase[0] = 'ten_concurrent_chats'
            with concurrent.futures.ThreadPoolExecutor(max_workers=10) as executor:
                result['ten_concurrent_chats'] = list(executor.map(chat, range(2000, 2010)))
            phase[0] = 'source_backup'
            for doc in catalog[:20]:
                window = call(base, f"/api/source/{doc['doc_uid']}/text?frm=1&to=80", token=user(0))
                if not window:
                    raise AssertionError('Empty source response')
            size = 0
            with request(base, '/api/admin/backup', token=admin) as response:
                while block := response.read(65536):
                    size += len(block)
            result['backup_bytes'] = size
            phase[0] = 'after_idle'
            time.sleep(.25)
            result['final'] = proc_stats(process.pid)
            result['metrics'] = call(base, '/api/admin/metrics', token=admin)
            if result['metrics']['running'] or result['metrics']['waiting']:
                raise AssertionError('Chat jobs were not released')
            finished.set()
            result['phase_peak_rss_kib'] = peaks
            result['success'] = True
            process.terminate()
            process.wait(timeout=10)
            log.close()
    finally:
        finished.set()
        if process is not None and process.poll() is None:
            process.kill()
            process.wait()
        mock.shutdown()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps({k: v for k, v in result.items() if k not in
                      ('sequential_turns', 'ten_concurrent_chats', 'packages')}, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
