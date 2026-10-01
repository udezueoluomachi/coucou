#!/usr/bin/env python3
"""
coucou-antigravity-hook.py — Hook relay between Antigravity CLI and Coucou.
Works on Windows (named pipe) and macOS (Unix domain socket).

Usage in hooks.json:
  "coucou": {
    "PreInvocation": [
      { "type": "command", "command": "python path/to/scripts/coucou-antigravity-hook.py PreInvocation" }
    ],
    "PreToolUse": [
      {
        "matcher": "run_command",
        "hooks": [
          { "type": "command", "command": "python path/to/scripts/coucou-antigravity-hook.py PermissionRequest", "timeout": 120 }
        ]
      },
      {
        "matcher": "*",
        "hooks": [
          { "type": "command", "command": "python path/to/scripts/coucou-antigravity-hook.py PreToolUse" }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          { "type": "command", "command": "python path/to/scripts/coucou-antigravity-hook.py PostToolUse" }
        ]
      }
    ],
    "Stop": [
      { "type": "command", "command": "python path/to/scripts/coucou-antigravity-hook.py Stop" }
    ]
  }
"""

import sys
import json
import os
import platform
import socket
import time

DROPPED_FIELDS = ["transcript_path", "transcriptPath", "artifactDirectoryPath"]
MAX_FIELD_LEN = 2000

def get_pipe_or_socket():
    if platform.system() == "Windows":
        try:
            import win32api
            import win32security
            token = win32security.OpenProcessToken(win32api.GetCurrentProcess(), win32security.TOKEN_QUERY)
            sid, _ = win32security.GetTokenInformation(token, win32security.TokenUser)
            sid_str = win32security.ConvertSidToStringSid(sid)
        except Exception:
            sid_str = os.environ.get("USERNAME", "user")
        return rf"\\.\pipe\coucou-{sid_str}"
    else:
        return os.path.expanduser("~/Library/Application Support/NotchBuddy/nb.sock")

def truncate_strings(obj):
    if isinstance(obj, dict):
        for k, v in obj.items():
            obj[k] = truncate_strings(v)
        return obj
    elif isinstance(obj, list):
        return [truncate_strings(x) for x in obj]
    elif isinstance(obj, str):
        if len(obj) > MAX_FIELD_LEN:
            return obj[:MAX_FIELD_LEN] + "…"
        return obj
    return obj

def talk_windows_pipe(pipe_name, payload_bytes, wait_for_answer=False):
    deadline = time.time() + 0.3
    handle = None
    while time.time() < deadline:
        try:
            handle = open(pipe_name, "r+b", buffering=0)
            break
        except Exception:
            time.sleep(0.02)
    if not handle:
        return None

    try:
        handle.write(payload_bytes + b"\n")
        handle.flush()
        if not wait_for_answer:
            handle.close()
            return None

        # Wait for answer
        answer = b""
        start_wait = time.time()
        while time.time() - start_wait < 110:
            chunk = handle.read(1024)
            if not chunk:
                break
            answer += chunk
            if b"\n" in chunk:
                break
        handle.close()
        return answer.decode("utf-8", errors="ignore").strip()
    except Exception:
        if handle:
            try:
                handle.close()
            except Exception:
                pass
        return None

def talk_unix_socket(socket_path, payload_bytes, wait_for_answer=False):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(118 if wait_for_answer else 0.3)
    try:
        s.connect(socket_path)
        s.sendall(payload_bytes + b"\n")
        if not wait_for_answer:
            s.close()
            return None
        
        chunks = []
        while True:
            chunk = s.recv(1024)
            if not chunk:
                break
            chunks.append(chunk)
            if b"\n" in chunk:
                break
        s.close()
        return b"".join(chunks).decode("utf-8", errors="ignore").strip()
    except Exception:
        try:
            s.close()
        except Exception:
            pass
        return None

def main():
    try:
        raw = sys.stdin.buffer.read()
        if not raw:
            return
        if raw.startswith(b"\xef\xbb\xbf"):
            raw = raw[3:]
        payload = json.loads(raw.decode("utf-8", errors="ignore"))
    except Exception:
        return

    arg_event = sys.argv[1] if len(sys.argv) > 1 else ""
    has_ask_flag = any(a in ("--ask", "--approve") for a in sys.argv)

    # Detect Antigravity CLI format
    is_antigravity = "conversationId" in payload or "toolCall" in payload

    if is_antigravity:
        payload["source"] = "antigravity"
        if "conversationId" in payload:
            payload["session_id"] = str(payload["conversationId"])
        if "workspacePaths" in payload and isinstance(payload["workspacePaths"], list) and payload["workspacePaths"]:
            payload["cwd"] = payload["workspacePaths"][0]
        elif "cwd" not in payload:
            payload["cwd"] = os.getcwd()

        if "toolCall" in payload and isinstance(payload["toolCall"], dict):
            tc = payload["toolCall"]
            if "name" in tc:
                payload["tool_name"] = tc["name"]
            if "args" in tc:
                payload["tool_input"] = tc["args"]

        if arg_event == "PermissionRequest" or has_ask_flag:
            event = "PermissionRequest"
        elif arg_event == "PreInvocation":
            event = "UserPromptSubmit"
        elif arg_event == "PreToolUse":
            event = "PreToolUse"
        elif arg_event == "PostToolUse":
            event = "PostToolUseFailure" if payload.get("error") else "PostToolUse"
        elif arg_event == "Stop":
            event = "StopFailure" if payload.get("error") else "Stop"
        elif arg_event:
            event = arg_event
        else:
            event = "PreToolUse"
    else:
        event = payload.get("hook_event_name") or arg_event or "PreToolUse"

    payload["hook_event_name"] = event

    for field in DROPPED_FIELDS:
        payload.pop(field, None)

    truncate_strings(payload)

    waits_for_answer = (event == "PermissionRequest")
    payload_bytes = json.dumps(payload).encode("utf-8")

    path = get_pipe_or_socket()
    decision = None
    if platform.system() == "Windows":
        decision = talk_windows_pipe(path, payload_bytes, waits_for_answer)
    else:
        decision = talk_unix_socket(path, payload_bytes, waits_for_answer)

    if waits_for_answer and decision:
        d = decision.strip().lower()
        if "allow" in d:
            if is_antigravity:
                sys.stdout.write(json.dumps({"decision": "allow"}) + "\n")
            else:
                out = {"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}}
                sys.stdout.write(json.dumps(out) + "\n")
        elif "deny" in d:
            if is_antigravity:
                sys.stdout.write(json.dumps({"decision": "deny", "reason": "Denied from Coucou"}) + "\n")
            else:
                out = {"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "deny", "message": "Denied from Coucou"}}}
                sys.stdout.write(json.dumps(out) + "\n")
        sys.stdout.flush()

if __name__ == "__main__":
    main()
    sys.exit(0)
