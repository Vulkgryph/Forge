#!/usr/bin/env python3
"""Black-box agent failure tests against a loopback-only fake provider.

Never contacts a real model or reads the user's configuration. Each case has
its own HOME/APPDATA/workspace and writes protocol evidence under --output.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import threading
import time


class Provider(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def log_message(self, *_):
        pass

    def do_GET(self):
        data = json.dumps({'data': [{'id': 'failure-fixture', 'context_length': 131072}]}).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.requests.append(request)
        first = len(self.server.requests) == 1
        mode = self.server.mode if first else 'ok'
        if mode in ('http503', 'http429', 'context_limit'):
            self.send_response({'http503': 503, 'http429': 429, 'context_limit': 400}[mode])
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(json.dumps({'error': {'message': 'context window exceeded' if mode == 'context_limit' else 'injected provider outage'}}).encode())
            return
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Connection', 'close')
        if mode == 'reset':
            self.send_header('Transfer-Encoding', 'chunked')
        self.end_headers()

        def event(value, kind=None):
            payload = (('event: ' + kind + '\n' if kind else '') + 'data: ' + json.dumps(value) + '\n\n').encode()
            if mode == 'reset':
                payload = ('%x\r\n' % len(payload)).encode() + payload + b'\r\n'
            self.wfile.write(payload)
            self.wfile.flush()

        def delta(content=None, tool=None, finish=None):
            body = {}
            if content is not None:
                body['content'] = content
            if tool is not None:
                body['tool_calls'] = [tool]
            event({'choices': [{'index': 0, 'delta': body, 'finish_reason': finish}]})

        try:
            if self.server.protocol == 'anthropic':
                event({'message': {'usage': {'input_tokens': 10}}}, 'message_start')
                if mode in ('tool_failure', 'truncated_tool'):
                    event({'index': 0, 'content_block': {'type': 'tool_use', 'id': 'fixture-tool', 'name': 'read_file', 'input': {}}}, 'content_block_start')
                    event({'index': 0, 'delta': {'type': 'input_json_delta', 'partial_json':
                          '{"path":"missing-fixture-file.txt"}' if mode == 'tool_failure' else '{"path":'}}, 'content_block_delta')
                    event({'index': 0}, 'content_block_stop')
                else:
                    event({'index': 0, 'delta': {'type': 'text_delta', 'text': 'RECOVERED' if mode == 'ok' else 'PARTIAL-FIXTURE'}}, 'content_block_delta')
                    if mode in ('eof', 'reset'):
                        self.close_connection = True
                        return
                    if mode in ('stall', 'cancel', 'restart'):
                        time.sleep(5)
                event({'delta': {'stop_reason': 'max_tokens' if mode in ('output_limit', 'truncated_tool') else ('tool_use' if mode == 'tool_failure' else 'end_turn')}, 'usage': {'output_tokens': 10}}, 'message_delta')
                event({}, 'message_stop')
                return
            if mode in ('tool_failure', 'truncated_tool'):
                delta(tool={'index': 0, 'id': 'fixture-tool', 'type': 'function',
                            'function': {'name': 'read_file', 'arguments':
                                         '{"path":"missing-fixture-file.txt"}' if mode == 'tool_failure' else '{"path":'}})
                delta(finish='tool_calls' if mode == 'tool_failure' else 'length')
            else:
                delta(content='RECOVERED' if mode == 'ok' else 'PARTIAL-FIXTURE')
                if mode in ('eof', 'reset'):
                    self.close_connection = True
                    return
                if mode in ('stall', 'cancel', 'restart'):
                    time.sleep(5)
                delta(finish='length' if mode == 'output_limit' else 'stop')
            self.wfile.write(b'data: [DONE]\n\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass


class Agent:
    def __init__(self, binary, home, resume=None):
        env = os.environ.copy()
        env.update(HOME=str(home), USERPROFILE=str(home), APPDATA=str(home/'AppData/Roaming'),
                   LOCALAPPDATA=str(home/'AppData/Local'), XDG_CONFIG_HOME=str(home/'.config'),
                   XDG_DATA_HOME=str(home/'.local/share'), XDG_CACHE_HOME=str(home/'.cache'))
        env['FORGE_CONFIG_FILE'] = str(home/'.config/forge/config.toml')
        args = [str(binary), '--headless']
        if resume:
            args += ['--resume-session', resume]
        self.stderr = (home/'stderr.log').open('a', encoding='utf-8')
        self.process = subprocess.Popen(args, cwd=home/'workspace', env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.stderr, text=True, encoding='utf-8')
        self.events = []
        self.q = queue.Queue()
        def read():
            for line in self.process.stdout:
                try:
                    message = json.loads(line)
                except ValueError:
                    message = {'type': 'invalid_json', 'line': line}
                self.events.append(message)
                self.q.put(message)
        threading.Thread(target=read, daemon=True).start()

    def send(self, kind, **fields):
        self.process.stdin.write(json.dumps({'type': kind, **fields}) + '\n')
        self.process.stdin.flush()

    def until(self, kinds, timeout=25):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                msg = self.q.get(timeout=min(0.2, deadline-time.monotonic()))
            except queue.Empty:
                if self.process.poll() is not None:
                    raise AssertionError('Agent exited unexpectedly: ' + str(self.process.returncode))
                continue
            if msg.get('type') in kinds:
                return msg
        raise AssertionError('Timed out waiting for ' + str(kinds))

    def close(self):
        if self.process.poll() is None:
            self.send('quit')
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.stderr.close()


def run_case(binary, output, mode, protocol):
    home = output/mode
    (home/'.config/forge').mkdir(parents=True)
    (home/'workspace').mkdir()
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Provider)
    server.daemon_threads = True
    server.mode, server.requests = mode, []
    server.protocol = protocol
    threading.Thread(target=server.serve_forever, daemon=True).start()
    (home/'.config/forge/config.toml').write_text(f'''
[models]
default = "fixture"
[[models.endpoints]]
name = "fixture"
base_url = "http://127.0.0.1:{server.server_port}/v1"
model_id = "failure-fixture"
max_context_tokens = 131072
max_output_tokens = 64
request_timeout_secs = 2
endpoint_type = "{protocol}"
api_key = "loopback-fixture-not-a-real-key"
[agent]
thinking_mode = false
auto_approve_reads = true
auto_approve_writes = false
max_history_messages = 200
compaction_threshold = 150
''')
    agent = Agent(binary, home)
    all_events = []
    result = {'case': mode, 'passed': False}
    try:
        init = agent.until({'init'})
        assert init.get('model_id') == 'failure-fixture', 'Refusing to send: agent did not load the isolated fixture configuration'
        agent.send('send_message', content='Exercise the failure fixture. Do not use external services.')
        if mode in ('cancel', 'restart'):
            agent.until({'assistant_token'})
            if mode == 'cancel':
                agent.send('cancel_run')
                end = agent.until({'cancelled', 'error', 'done'})
                assert end['type'] == 'cancelled', end
            else:
                session_id = init['session_id']
                agent.process.kill()
                agent.process.wait()
                all_events.extend(agent.events)
                agent.close()
                agent = Agent(binary, home, resume=session_id)
                agent.until({'init'})
                resumed = agent.until({'session_loaded'})
                assert resumed.get('message_count', 0) >= 1, resumed
        else:
            end = agent.until({'done', 'error', 'cancelled'})
            if mode in ('eof', 'reset', 'output_limit', 'truncated_tool', 'stall', 'http429', 'context_limit'):
                assert end['type'] == 'error', end
            else:
                assert end['type'] == 'done', end
            if mode == 'truncated_tool':
                assert not any(e['type'] == 'tool_request' for e in agent.events), 'Truncated tool was dispatched'
            if mode == 'tool_failure':
                assert any(e['type'] == 'tool_result' and not e['success'] for e in agent.events), 'Failed tool was not reported'
            if mode == 'http503':
                assert len(server.requests) >= 2, 'Transient failure was not retried'
        # Every fault must leave the session usable for a subsequent turn.
        server.mode = 'ok'
        agent.send('send_message', content='Recover now.')
        end = agent.until({'done', 'error', 'cancelled'})
        assert end['type'] == 'done', end
        assert any(e.get('content') == 'RECOVERED' for e in agent.events), 'No recovered answer'
        if mode in ('eof', 'reset', 'output_limit', 'stall'):
            messages = server.requests[-1]['messages']
            partials = [m for m in messages if m.get('role') == 'assistant' and 'PARTIAL-FIXTURE' in json.dumps(m.get('content'))]
            assert len(partials) == 1, 'Partial answer must be preserved exactly once'
        result['passed'] = True
    except Exception as exc:
        result['error'] = repr(exc)
    finally:
        agent.close()
        all_events.extend(agent.events)
        (home/'events.json').write_text(json.dumps(all_events, indent=2))
        (home/'requests.json').write_text(json.dumps(server.requests, indent=2))
        server.shutdown()
        server.server_close()
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cases', default='eof,reset,output_limit,truncated_tool,tool_failure,http503,http429,context_limit,stall,cancel,restart')
    parser.add_argument('--protocol', choices=['open_ai', 'anthropic'], default='open_ai')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    results = []
    for mode in args.cases.split(','):
        result = run_case(args.agent.resolve(), args.output.resolve(), mode, args.protocol)
        results.append(result)
        print(json.dumps(result), flush=True)
        (args.output/'results.json').write_text(json.dumps(results, indent=2))
    raise SystemExit(0 if all(r['passed'] for r in results) else 1)
