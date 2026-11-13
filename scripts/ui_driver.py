#!/usr/bin/env python3
"""Client for the opt-in AppKit UI driver (one JSON request per connection)."""

import argparse
import json
from pathlib import Path
import socket
import time


class Driver:
    def __init__(self, socket_path, state_path=None):
        self.socket = str(socket_path)
        self.state_path = Path(state_path) if state_path else None

    def send(self, op, **values):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(25)
            stream.connect(self.socket)
            stream.sendall((json.dumps(dict(op=op, **values), ensure_ascii=False) + '\n').encode())
            chunks = bytearray()
            while not chunks.endswith(b'\n'):
                data = stream.recv(65536)
                if not data:
                    break
                chunks.extend(data)
        result = json.loads(chunks)
        if result.get('error'):
            raise RuntimeError(result['error'])
        return result

    def key(self, key):
        return self.send('key', key=key)

    def text(self, text):
        return self.send('text', text=text)

    def click(self, x, y, **values):
        return self.send('click', x=x, y=y, **values)

    def query(self, text):
        self.key('cmd-f')
        self.key('cmd-a')
        if text:
            self.text(text)
        else:
            self.key('backspace')

    def state(self):
        if not self.state_path:
            raise RuntimeError('No UI state file configured')
        for _ in range(20):
            try:
                return json.loads(self.state_path.read_text(encoding='utf-8'))
            except (FileNotFoundError, json.JSONDecodeError):
                time.sleep(0.01)
        raise RuntimeError('UI state file is unavailable')

    def wait(self, predicate, timeout=12):
        deadline = time.monotonic() + timeout
        last = None
        while time.monotonic() < deadline:
            last = self.state()
            if predicate(last):
                return last
            time.sleep(0.025)
        raise AssertionError(f'UI condition timed out. Last state: {json.dumps(last, ensure_ascii=False)}')

    def wait_query(self, text, **values):
        self.query(text)
        return self.wait(lambda s: s['query'] == text and not s['busy'] and s['totalKnown'], **values)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('socket')
    parser.add_argument('request', help='JSON, for example {"op":"key","key":"cmd-f"}')
    args = parser.parse_args()
    request = json.loads(args.request)
    print(json.dumps(Driver(args.socket).send(**request), ensure_ascii=False))
