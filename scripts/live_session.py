#!/usr/bin/env python3
"""Manual integration-test bridge for lcu's persistent JSON-lines CLI.

Run: python3 scripts/live_session.py serve
Send: python3 scripts/live_session.py send '{"command":"start"}'
Screenshots are saved only to explicitly supplied output paths. Stop with the
targeted lcu stop command or Ctrl+C. This helper is not a production transport.
"""
import argparse
import asyncio
import json
import os
from pathlib import Path
import socket
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SOCKET = Path(os.environ.get('LCU_TEST_SOCKET', ROOT / '.local' / 'live-test.sock'))

async def serve(desktop):
    SOCKET.parent.mkdir(exist_ok=True)
    SOCKET.unlink(missing_ok=True)
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--format-version=1'], cwd=ROOT))
    binary = os.environ.get('LCU_BINARY', str(Path(metadata['target_directory']) / 'debug/lcu'))
    child = await asyncio.create_subprocess_exec(binary, 'session', '--desktop', desktop,
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, limit=64 * 1024 * 1024)
    child.stdin.write(b'{"command":"claim"}\n')
    await child.stdin.drain()
    claim = json.loads(await child.stdout.readline())
    if 'error' in claim:
        child.stdin.close()
        await child.wait()
        raise RuntimeError(claim['error'])
    lock = asyncio.Lock()
    async def connection(reader, writer):
        try:
            line = await reader.readline()
            request = json.loads(line)
            # Test EOF while a previous request is still awaiting its response.
            # Do not take that request's lock or close stdout prematurely.
            if request == {'disconnect': True}:
                child.stdin.close()
                code = await child.wait()
                response = (json.dumps({'disconnected': True, 'exit_code': code}) + '\n').encode()
            else:
                async with lock:
                    child.stdin.write(line)
                    await child.stdin.drain()
                    response = await child.stdout.readline()
            writer.write(response or b'{"error":"lcu exited"}\n')
            await writer.drain()
        except Exception as e:
            writer.write((json.dumps({'error': str(e)}) + '\n').encode())
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()
    server = await asyncio.start_unix_server(connection, path=SOCKET)
    os.chmod(SOCKET, 0o600)
    print('LIVE_SESSION_READY', flush=True)
    try:
        async with server:
            await server.serve_forever()
    finally:
        child.stdin.close()
        if child.returncode is None:
            child.terminate()
        await child.wait()
        SOCKET.unlink(missing_ok=True)

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['serve', 'send'])
    parser.add_argument('request', nargs='?')
    parser.add_argument('--desktop', default='main')
    args = parser.parse_args()
    if args.mode == 'serve':
        try: asyncio.run(serve(args.desktop))
        except KeyboardInterrupt: pass
    else:
        json.loads(args.request)
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(180)
            client.connect(str(SOCKET))
            client.sendall((args.request + '\n').encode())
            with client.makefile('r') as stream:
                sys.stdout.write(stream.readline())
