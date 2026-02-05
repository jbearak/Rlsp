#!/usr/bin/env python3
"""
Profile Raven LSP startup latency.

This script simulates VS Code opening a workspace and specific files,
measuring time to first diagnostic.
"""

import json
import subprocess
import sys
import time
import os
import threading
import select
from pathlib import Path
from urllib.parse import unquote, urlparse

def send_message(proc, msg):
    """Send an LSP message to the server."""
    content = json.dumps(msg)
    content_bytes = content.encode('utf-8')
    header = f"Content-Length: {len(content_bytes)}\r\n\r\n"
    proc.stdin.write(header.encode('utf-8'))
    proc.stdin.write(content_bytes)
    proc.stdin.flush()

def read_stderr(proc, output_lines, start_time):
    """Read stderr in a thread."""
    while True:
        line = proc.stderr.readline()
        if not line:
            break
        decoded = line.decode('utf-8', errors='replace').strip()
        if decoded:
            output_lines.append((time.time() - start_time, decoded))
            # Only print relevant lines
            lower = decoded.lower()
            if any(x in lower for x in ['perf', 'init', 'scan', 'package', 'background', 'diag']):
                print(f"  [{(time.time() - start_time)*1000:.0f}ms] {decoded}", file=sys.stderr)

def read_message(proc, timeout=30):
    """Read an LSP message from the server."""
    header = b""
    deadline = time.time() + timeout

    while time.time() < deadline:
        remaining = deadline - time.time()
        if remaining <= 0:
            return None
        if not select.select([proc.stdout], [], [], min(0.1, remaining))[0]:
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

    # Read the body with the same timeout/deadline enforcement as the header.
    body = b""
    while len(body) < content_length and time.time() < deadline:
        remaining = deadline - time.time()
        if remaining <= 0:
            return None
        if not select.select([proc.stdout], [], [], min(0.1, remaining))[0]:
            continue
        to_read = min(4096, content_length - len(body))
        chunk = proc.stdout.read(to_read)
        if not chunk:
            return None
        body += chunk

    if len(body) != content_length:
        return None

    return json.loads(body.decode('utf-8'))

def main():
    workspace = Path(os.path.expanduser("~/repos/worldwide")).resolve()
    files_to_open = [
        workspace / "oos.r",
        workspace / "validation_functions/collate.r",
    ]

    # Verify files exist
    for f in files_to_open:
        if not f.exists():
            print(f"Error: File not found: {f}")
            sys.exit(1)

    raven_path = Path(os.path.expanduser("~/repos/raven/target/release/raven")).resolve()

    # Preflight check: fail fast with a clear message.
    if not raven_path.exists():
        print(f"Error: Raven binary not found: {raven_path}", file=sys.stderr)
        sys.exit(1)
    if not os.access(raven_path, os.X_OK):
        print(f"Error: Raven binary is not executable: {raven_path}", file=sys.stderr)
        sys.exit(1)

    # Start Raven with perf logging
    env = os.environ.copy()
    env["RAVEN_PERF"] = "1"
    env["RUST_LOG"] = "raven=info"

    print("Starting Raven LSP server...")
    start_time = time.time()
    stderr_lines = []

    proc = subprocess.Popen(
        [str(raven_path), "--stdio"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=str(workspace),
        env=env,
        bufsize=0,  # Unbuffered binary mode
    )

    # Start stderr reader thread
    stderr_thread = threading.Thread(target=read_stderr, args=(proc, stderr_lines, start_time), daemon=True)
    stderr_thread.start()

    spawn_time = time.time()
    print(f"  Process spawned: {(spawn_time - start_time)*1000:.1f}ms")

    # Send initialize
    workspace_uri = workspace.as_uri()

    send_message(proc, {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": os.getpid(),
            "rootUri": workspace_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {"relatedInformation": True}
                }
            },
            "workspaceFolders": [
                {"uri": workspace_uri, "name": "worldwide"}
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

    # Wait for initialize response
    resp = read_message(proc)
    if resp is None:
        print("Error: initialize response timed out", file=sys.stderr)
        proc.kill()
        sys.exit(1)
    init_response_time = time.time()
    print(f"  Initialize response: {(init_response_time - start_time)*1000:.1f}ms")

    # Send initialized notification
    send_message(proc, {
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    })

    initialized_sent_time = time.time()
    print(f"  Initialized sent: {(initialized_sent_time - start_time)*1000:.1f}ms")

    # Give a moment for initialization to start
    time.sleep(0.1)

    # Open the files
    for file_path in files_to_open:
        with open(file_path, 'r') as f:
            content = f.read()

        file_uri = file_path.as_uri()
        send_message(proc, {
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "r",
                    "version": 1,
                    "text": content
                }
            }
        })
        open_time = time.time()
        print(f"  Opened {file_path.name}: {(open_time - start_time)*1000:.1f}ms")

    # Wait for diagnostics
    print("\nWaiting for diagnostics...")
    first_diagnostic_time = None
    all_diagnostics_time = None
    diagnostics_by_file = {}

    timeout = 30
    deadline = time.time() + timeout

    while time.time() < deadline:
        msg = read_message(proc, timeout=1)
        if msg is None:
            continue

        if msg.get("method") == "textDocument/publishDiagnostics":
            uri = msg["params"]["uri"]
            diags = msg["params"]["diagnostics"]
            now = time.time()

            if first_diagnostic_time is None:
                first_diagnostic_time = now
                print(f"\n  First diagnostic: {(now - start_time)*1000:.1f}ms (total)")
                print(f"    From initialized: {(now - initialized_sent_time)*1000:.1f}ms")

            basename = Path(unquote(urlparse(uri).path)).name
            diagnostics_by_file[basename] = len(diags)
            print(f"    {basename}: {len(diags)} diagnostics @ {(now - start_time)*1000:.1f}ms")

            # Check if we have diagnostics for all opened files
            opened_basenames = {f.name for f in files_to_open}
            if opened_basenames.issubset(diagnostics_by_file.keys()):
                all_diagnostics_time = now
                break

    # Summary
    print("\n" + "="*60)
    print("TIMING SUMMARY")
    print("="*60)
    print(f"  Process spawn:         {(spawn_time - start_time)*1000:>8.1f}ms")
    print(f"  Initialize response:   {(init_response_time - start_time)*1000:>8.1f}ms")
    print(f"  Initialized sent:      {(initialized_sent_time - start_time)*1000:>8.1f}ms")
    if first_diagnostic_time:
        print(f"  First diagnostic:      {(first_diagnostic_time - start_time)*1000:>8.1f}ms")
        print(f"    (from initialized):  {(first_diagnostic_time - initialized_sent_time)*1000:>8.1f}ms")
    if all_diagnostics_time:
        print(f"  All files diagnosed:   {(all_diagnostics_time - start_time)*1000:>8.1f}ms")

    print("\nDiagnostics by file:")
    for fname, count in diagnostics_by_file.items():
        print(f"  {fname}: {count}")

    # Send shutdown
    send_message(proc, {"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
    time.sleep(0.2)
    send_message(proc, {"jsonrpc": "2.0", "method": "exit", "params": None})

    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()

if __name__ == "__main__":
    main()
