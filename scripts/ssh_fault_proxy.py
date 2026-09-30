#!/usr/bin/env python3
"""Local test-only SSH relay. A marker interrupts only this relay's sockets.

Also serves the mock model endpoint the relayed agent talks to, on :22284.

That fixture used to be imported from `scripts/test_resilience.py`. The Python
resilience harness was ported to Rust in 955e562 and the file went with it,
which left this script importing something that no longer existed — it raised
FileNotFoundError before either listener was created, so the reproduction path
FAILURE-TESTING.md documents could not run at all. The provider is inlined
here now, covering the one mode this script drives.
"""
import argparse
import http.server
import json
import socket
import threading
import time
from pathlib import Path
from http.server import ThreadingHTTPServer

parser = argparse.ArgumentParser()
parser.add_argument('--marker', type=Path, required=True)
args = parser.parse_args()
active = set()
lock = threading.Lock()
pause_until = 0


def copy(source, destination):
    try:
        while True:
            data = source.recv(65536)
            if not data:
                break
            destination.sendall(data)
    except OSError:
        pass
    finally:
        for sock in (source, destination):
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            sock.close()
            with lock:
                active.discard(sock)


def monitor():
    global pause_until
    while True:
        if args.marker.exists():
            args.marker.unlink()
            pause_until = time.monotonic() + 2
            with lock:
                sockets = list(active)
            for sock in sockets:
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            print('Injected SSH disconnect', flush=True)
        time.sleep(.02)


class Provider(http.server.BaseHTTPRequestHandler):
    """An OpenAI-compatible endpoint that truncates its first response.

    Only what this script needs: `output_limit` on the first request, `ok`
    after, over the OpenAI streaming protocol. The Rust harness at
    forge-agent/tests/resilience.rs covers the other modes and protocols.
    """

    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def do_GET(self):
        if self.path == '/reset':
            # Clears the recorded requests. It deliberately does not reset
            # `mode`, which stays pinned to output_limit for this script's
            # lifetime — the first request after a reset truncates again.
            self.server.requests.clear()
        data = json.dumps(
            {'data': [{'id': 'failure-fixture', 'context_length': 131072}]}
        ).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        length = int(self.headers['Content-Length'])
        self.server.requests.append(json.loads(self.rfile.read(length)))
        truncate = len(self.server.requests) == 1

        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Connection', 'close')
        self.end_headers()

        def delta(body, finish=None):
            payload = 'data: ' + json.dumps(
                {'choices': [{'index': 0, 'delta': body, 'finish_reason': finish}]}
            ) + '\n\n'
            self.wfile.write(payload.encode())
            self.wfile.flush()

        try:
            delta({'content': 'PARTIAL-FIXTURE' if truncate else 'RECOVERED'})
            delta({}, finish='length' if truncate else 'stop')
            self.wfile.write(b'data: [DONE]\n\n')
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass


provider = ThreadingHTTPServer(('127.0.0.1', 22284), Provider)
provider.requests, provider.mode, provider.protocol = [], 'output_limit', 'open_ai'
threading.Thread(target=provider.serve_forever, daemon=True).start()
threading.Thread(target=monitor, daemon=True).start()
with socket.socket() as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(('127.0.0.1', 22283))
    listener.listen()
    print('SSH relay 127.0.0.1:22283 -> :22282; model fixture :22284', flush=True)
    while True:
        client, _ = listener.accept()
        if time.monotonic() < pause_until:
            client.close()
            continue
        try:
            upstream = socket.create_connection(('127.0.0.1', 22282))
        except OSError as e:
            # The relay outlives its upstream. sshd on :22282 not being there
            # is the ordinary case at startup and between test runs, and an
            # unhandled refusal here took the whole script down — including
            # the model fixture on :22284, which has nothing to do with ssh.
            print(f'upstream :22282 refused ({e}); dropping this connection',
                  flush=True)
            client.close()
            continue
        with lock:
            active.update((client, upstream))
        threading.Thread(target=copy, args=(client, upstream), daemon=True).start()
        threading.Thread(target=copy, args=(upstream, client), daemon=True).start()
