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

def get_terminal_pids():
    pids = []
    try:
        ppid = os.getppid()
        if ppid:
            pids.append(ppid)
        if platform.system() == "Windows":
            import ctypes
            from ctypes import wintypes
            TH32CS_SNAPPROCESS = 0x00000002
            class PROCESSENTRY32(ctypes.Structure):
                _fields_ = [
                    ("dwSize", wintypes.DWORD),
                    ("cntUsage", wintypes.DWORD),
                    ("th32ProcessID", wintypes.DWORD),
                    ("th32DefaultHeapID", ctypes.c_void_p),
                    ("thModuleID", wintypes.DWORD),
                    ("cntThreads", wintypes.DWORD),
                    ("thParentProcessID", wintypes.DWORD),
                    ("pcPriClassBase", ctypes.c_long),
                    ("dwFlags", wintypes.DWORD),
                    ("szExeFile", ctypes.c_char * 260),
                ]
            kernel32 = ctypes.windll.kernel32
            hSnap = kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            if hSnap and hSnap != -1:
                pe = PROCESSENTRY32()
                pe.dwSize = ctypes.sizeof(PROCESSENTRY32)
                parents = {}
                if kernel32.Process32First(hSnap, ctypes.byref(pe)):
                    while True:
                        parents[pe.th32ProcessID] = pe.thParentProcessID
                        if not kernel32.Process32Next(hSnap, ctypes.byref(pe)):
                            break
                kernel32.CloseHandle(hSnap)
                curr = pids[0]
                for _ in range(5):
                    p = parents.get(curr)
                    if p and p > 0 and p not in pids:
                        pids.append(p)
                        curr = p
                    else:
                        break
    except Exception:
        pass
    return pids

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

        payload["terminal_pids"] = get_terminal_pids()

        if "toolCall" in payload and isinstance(payload["toolCall"], dict):
            tc = payload["toolCall"]
            if "name" in tc:
                payload["tool_name"] = tc["name"]
            if "args" in tc:
                payload["tool_input"] = tc["args"]

        tool_name = payload.get("tool_name", "")
        if arg_event == "PermissionRequest" or has_ask_flag or (arg_event == "PreToolUse" and tool_name == "run_command"):
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
            event = "PermissionRequest" if tool_name == "run_command" else "PreToolUse"
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

    if is_antigravity:
        if waits_for_answer:
            if decision and "allow" in decision.strip().lower():
                args = payload.get("tool_input") or {}
                tool_name = payload.get("tool_name", "")
                overrides = []
                if tool_name == "run_command":
                    cmd = args.get("CommandLine") or args.get("command") or ""
                    if cmd:
                        overrides.append(f"command({cmd})")
                    overrides.append("command(*)")
                elif tool_name in ("write_to_file", "replace_file_content"):
                    target = args.get("TargetFile") or args.get("path") or ""
                    if target:
                        overrides.append(f"file({target})")
                    overrides.append("file(*)")
                else:
                    overrides.extend(["command(*)", "file(*)"])

                out = {
                    "decision": "allow",
                    "permissionOverrides": overrides,
                    "allowTool": True,
                }
                sys.stdout.write(json.dumps(out) + "\n")
            elif decision and "deny" in decision.strip().lower():
                sys.stdout.write(json.dumps({"decision": "deny", "reason": "Denied from Coucou"}) + "\n")
            else:
                # Coucou closed, ignored, or timed out -> prompt in terminal
                sys.stdout.write(json.dumps({"decision": "ask"}) + "\n")
        else:
            if arg_event == "PreToolUse" or event == "PreToolUse":
                sys.stdout.write(json.dumps({"decision": "allow", "allowTool": True}) + "\n")
            else:
                sys.stdout.write("{}\n")
        sys.stdout.flush()
    elif waits_for_answer and decision:
        d = decision.strip().lower()
        if "allow" in d:
            out = {"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}}
            sys.stdout.write(json.dumps(out) + "\n")
        elif "deny" in d:
            out = {"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "deny", "message": "Denied from Coucou"}}}
            sys.stdout.write(json.dumps(out) + "\n")
        sys.stdout.flush()

if __name__ == "__main__":
    main()
    sys.exit(0)
