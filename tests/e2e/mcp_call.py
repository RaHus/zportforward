#!/usr/bin/env python3
"""Call one tool on `devfwd mcp` the way an editor would.

    mcp_call.py <devfwd binary> <tool> [json arguments]

Prints the tool's text result; exits 1 if the tool reported an error.
"""
import json
import subprocess
import sys

binary, tool = sys.argv[1], sys.argv[2]
arguments = json.loads(sys.argv[3]) if len(sys.argv) > 3 else {}
proc = subprocess.Popen([binary, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)


def send(message):
    proc.stdin.write(json.dumps(message) + "\n")
    proc.stdin.flush()


def reply(request_id):
    for line in proc.stdout:
        message = json.loads(line)
        if message.get("id") == request_id:
            return message
    sys.exit("devfwd mcp closed the connection")


send({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
    "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "0"}}})
reply(0)
send({"jsonrpc": "2.0", "method": "notifications/initialized"})
send({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": arguments}})
result = reply(1)["result"]
proc.stdin.close()
proc.wait(timeout=10)
print(result["content"][0]["text"])
sys.exit(1 if result.get("isError") else 0)
