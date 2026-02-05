#!/usr/bin/env python3
"""
Simpler profile - just initialize and track stderr timing.
"""

import json
import subprocess
import sys
import time
import os
import threading

def send_message(proc, msg):
    content = json.dumps(msg)
    content_bytes = content.encode('utf-8')
    header = f"Content-Length: {len(content_bytes)}\r\n\r\n"
    proc.stdin.write(header.encode('utf-8'))
    proc.stdin.write(content_bytes)
    proc.stdin.flush()

def read_stderr(proc, output_lines):
    """Read stderr in a thread."""
    while True:
        line = proc.stderr.readline()
        if not line:
            break
        decoded = line.decode('utf-8', errors='replace').strip()
        if decoded:
            output_lines.append((time.time(), decoded))
            print(f"[stderr] {decoded}", file=sys.stderr)

def read_message(proc, timeout=60):
    """Read an LSP message from the server."""
    import select

    header = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if not select.select([proc.stdout], [], [], 0.1)[0]:
            continue
        char = proc.stdout.read(1)
        if not char:
            return None
        header += char
        if header.endswith(b"\r\n\r\n"):
            break
    else:
        return None

    header_str = header.decode('utf-8')
    content_length = 0
    for line in header_str.strip().split("\r\n"):
        if line.lower().startswith("content-length:"):
            content_length = int(line.split(":")[1].strip())

    if content_length == 0:
        return None

    content = proc.stdout.read(content_length)
    return json.loads(content.decode('utf-8'))

def main():
    workspace = os.path.expanduser(os.environ.get("RAVEN_WORKSPACE", "~/repos/worldwide"))
    raven_path = os.path.expanduser(os.environ.get("RAVEN_PATH", "~/repos/raven/target/release/raven"))

    env = os.environ.copy()
    env["RAVEN_PERF"] = "verbose"
    env["RUST_LOG"] = "raven=trace"

    print(f"Starting Raven LSP server with workspace: {workspace}")
    start_time = time.time()
    stderr_lines = []

    proc = subprocess.Popen(
        [raven_path, "--stdio"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=workspace,
        env=env,
    )

    # Start stderr reader thread
    stderr_thread = threading.Thread(target=read_stderr, args=(proc, stderr_lines), daemon=True)
    stderr_thread.start()

    spawn_time = time.time()
    print(f"Process spawned: {(spawn_time - start_time)*1000:.1f}ms")

    # Send initialize
    print("Sending initialize...")
    init_send_time = time.time()
    send_message(proc, {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": os.getpid(),
            "rootUri": f"file://{workspace}",
            "capabilities": {},
            "workspaceFolders": [
                {"uri": f"file://{workspace}", "name": "worldwide"}
            ],
            "initializationOptions": {
                "crossFile": {
                    "enabled": True,
                    "indexWorkspace": True,
                    "packages": {"enabled": True}
                }
            }
        }
    })

    print("Waiting for initialize response...")
    resp = read_message(proc, timeout=120)
    if resp is None:
        print("Error: initialize response timed out", file=sys.stderr)
        proc.kill()
        sys.exit(1)
    init_response_time = time.time()
    print(f"Initialize response received: {(init_response_time - start_time)*1000:.1f}ms")
    print(f"  Response id: {resp.get('id')}")

    # Send initialized
    print("Sending initialized notification...")
    send_message(proc, {
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    })

    # Wait a bit and collect diagnostics
    print("Waiting 5 seconds for async work...")
    time.sleep(5)

    # Shutdown
    print("Sending shutdown...")
    send_message(proc, {"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    time.sleep(0.5)
    send_message(proc, {"jsonrpc": "2.0", "method": "exit", "params": None})
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()

    print("\n" + "="*60)
    print("TIMING SUMMARY")
    print("="*60)
    print(f"  Process spawn: {(spawn_time - start_time)*1000:.1f}ms")
    print(f"  Initialize sent: {(init_send_time - start_time)*1000:.1f}ms")
    print(f"  Initialize response: {(init_response_time - start_time)*1000:.1f}ms")

    print("\n" + "="*60)
    print("STDERR TIMELINE (first 50 lines)")
    print("="*60)
    for ts, line in stderr_lines[:50]:
        print(f"[{(ts - start_time)*1000:8.1f}ms] {line}")

if __name__ == "__main__":
    main()
