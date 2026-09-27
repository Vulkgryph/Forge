#!/usr/bin/env python3
"""Local test-only SSH relay. A marker interrupts only this relay's sockets."""
import argparse
import importlib.util
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


spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('test_resilience.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Provider(fixture.Provider):
    def do_GET(self):
        if self.path == '/reset':
            self.server.requests.clear()
        super().do_GET()


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
        upstream = socket.create_connection(('127.0.0.1', 22282))
        with lock:
            active.update((client, upstream))
        threading.Thread(target=copy, args=(client, upstream), daemon=True).start()
        threading.Thread(target=copy, args=(upstream, client), daemon=True).start()
