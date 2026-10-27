"""Run the real local application against a loopback-only model fixture; never use a real key or user state."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

exe = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/karuo-cleaner').resolve())
mode = 'valid'
requests = []
arrived = threading.Event()
release = threading.Event()

class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(body)
        selected_mode = mode
        if selected_mode == 'wait':
            arrived.set()
            release.wait(10)
        self.send_response(500 if selected_mode == 'failure' else 200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        result = {'choices': [{'message': {'content': json.dumps({'reply': '请保留重要资料，当前仅生成建议。', 'action': 'none', 'refs': []})}}]}
        if selected_mode != 'unknown':
            result['usage'] = {'prompt_tokens': 1000, 'completion_tokens': 500, 'total_tokens': 1500}
        self.wfile.write(json.dumps(result).encode())

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
threading.Thread(target=server.serve_forever, daemon=True).start()
with tempfile.TemporaryDirectory(prefix='cm-ai-', dir='/private/tmp') as raw_root:
    root = Path(raw_root).resolve()
    env = dict(os.environ, HOME=str(root))
    log = (root / 'app.log').open('w')
    process = None

    def api(request):
        r = subprocess.run([exe, '--api', '-'], input=json.dumps(request), env=env, capture_output=True, text=True, timeout=20)
        if r.returncode:
            raise AssertionError(r.stderr)
        value = json.loads(r.stdout)
        if value.get('error') and value.get('event') != 'ai_chat':
            raise AssertionError(value)
        return value

    def start():
        global process
        process = subprocess.Popen([exe], env=env, stdout=log, stderr=log)
        for _ in range(100):
            if process.poll() is not None:
                raise AssertionError('Application exited before its API was ready: '+(root/'app.log').read_text()[-2000:])
            if (root / 'Library/Application Support/KaruoCleaner/control.sock').exists():
                try:
                    return api({'cmd': 'ready'})
                except AssertionError:
                    pass
            time.sleep(.05)
        raise AssertionError('API not ready')

    def chat(request_id, billing='demo', module='junk', subpage=None):
        return api({'cmd': 'ai_chat', 'module': module, 'subpage': subpage, 'text': '分析当前内容并保留重要资料', 'chat_request_id': request_id, 'ai_billing': billing})

    def wallet():
        return api({'cmd': 'billing', 'action': 'status'})

    try:
        start()
        api({'cmd': 'ai_save', 'base_url': f'http://127.0.0.1:{server.server_port}/v1', 'model': 'fixture-model', 'api_key': 'fixture-not-a-real-key'})
        byok = chat('byok-1', 'byok')
        assert byok['source'] == 'ai' and 'billing' not in byok and wallet()['credits'] == 0
        denied = chat('no-trial')
        assert denied['billing_required'] is False and len(requests) == 1
        api({'cmd': 'billing', 'action': 'mode', 'enabled': True})
        denied = chat('trial-required')
        assert denied['billing_required'] and len(requests) == 1
        api({'cmd': 'billing', 'action': 'trial', 'request_id': 'trial'})
        response = chat('charged')
        assert response['billing']['credits'] == -2.5 and wallet()['credits'] == 97.5
        n = len(requests)
        duplicate = chat('charged')
        assert duplicate.get('error') and len(requests) == n and wallet()['credits'] == 97.5
        mode = 'failure'
        assert chat('failed').get('error') and wallet()['credits'] == 97.5
        mode = 'unknown'
        assert chat('unknown')['billing']['usage_known'] is False and wallet()['credits'] == 97.5
        mode = 'wait'
        result = []
        worker = threading.Thread(target=lambda: result.append(chat('cancelled')))
        worker.start()
        assert arrived.wait(5)
        api({'cmd': 'ai_cancel', 'chat_request_id': 'cancelled', 'ai_billing': 'demo'})
        release.set()
        worker.join(15)
        assert result and result[0]['billing']['cancelled'] and wallet()['credits'] == 97.5
        mode = 'valid'
        running = chat('running-scope', 'byok', 'optimize', 'running')
        startup = chat('startup-scope', 'byok', 'optimize', 'startup')
        assert running['subpage'] == 'running' and startup['subpage'] == 'startup'
        for body in requests[-2:]:
            context = json.loads(body['messages'][-1]['content'])
            assert context['subpage'] in ('running', 'startup') and 'items' in context
            assert 'scan' not in context
        state_file = root / 'Library/Application Support/KaruoCleaner/billing-demo.json'
        data = json.loads(state_file.read_text())
        data['milli_credits'] = 0
        state_file.write_text(json.dumps(data))
        n = len(requests)
        assert chat('insufficient')['billing_required'] and len(requests) == n
        assert api({'cmd': 'ready'})['event'] == 'ready'
        api({'cmd': 'billing', 'action': 'topup', 'yuan': 1, 'request_id': 'topup'})
        assert wallet()['credits'] == 100
        api({'cmd': 'billing', 'action': 'purchase', 'plan': 'annual', 'request_id': 'annual'})
        assert wallet()['plan'] == 'annual' and wallet()['credits'] == 1100
        api({'cmd': 'billing', 'action': 'purchase', 'plan': 'lifetime', 'request_id': 'lifetime'})
        assert wallet()['plan'] == 'lifetime' and wallet()['credits'] == 3100
        process.terminate()
        process.wait(10)
        start()
        restored = api({'cmd': 'billing', 'action': 'restore'})
        assert restored['plan'] == 'lifetime' and restored['credits'] == 3100
        n = len(requests)
        assert chat('charged').get('error') and len(requests) == n and wallet()['credits'] == 3100
        print('PASS native API: BYOK separation, trial, actual usage, duplicate/restart, failure, unknown usage, cancellation, insufficient balance, topup, annual, lifetime, restore, module-scoped management')
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            process.wait(10)
        log.close()
server.shutdown()
