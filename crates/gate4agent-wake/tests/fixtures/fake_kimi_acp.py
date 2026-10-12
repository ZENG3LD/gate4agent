#!/usr/bin/env python3
"""Minimal fake `kimi acp`: NDJSON JSON-RPC on stdio. No real model.
Commands: initialize, session/new, session/resume, session/set_mode, session/prompt,
session/request_permission (so the host's cancel path is exercised).
"""
import json, sys

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

sessions = {}
next_id = 1
prompt_log = []

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1}})
    elif method == "session/new":
        sid = f"fake-{next_id}"
        next_id += 1
        sessions[sid] = True
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": sid}})
    elif method == "session/resume":
        sid = params.get("sessionId")
        if sid in sessions:
            send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": sid}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "error": {"message": f"Unknown sessionId: {sid}"}})
    elif method == "session/set_mode":
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "session/prompt":
        # Emit a permission ask first so the host must cancel it.
        ask_id = 900000 + len(prompt_log)
        send({
            "jsonrpc": "2.0",
            "id": ask_id,
            "method": "session/request_permission",
            "params": {"toolCall": {"title": "shell", "kind": "execute"}},
        })
        # Wait for the cancel answer before finishing the turn.
        # (Reader on host answers asynchronously; we just continue.)
        text = ""
        for part in params.get("prompt") or []:
            if isinstance(part, dict) and part.get("type") == "text":
                text += part.get("text") or ""
        prompt_log.append(text)
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
    else:
        send({"jsonrpc": "2.0", "id": mid, "error": {"message": f"unknown method {method}"}})
