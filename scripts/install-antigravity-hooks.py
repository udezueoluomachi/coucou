#!/usr/bin/env python3
"""
install-antigravity-hooks.py — Installs Coucou lifecycle hooks for Antigravity CLI.

Usage:
  python scripts/install-antigravity-hooks.py              # installs to ~/.gemini/config/hooks.json
  python scripts/install-antigravity-hooks.py --local      # installs to ./.agents/hooks.json
  python scripts/install-antigravity-hooks.py --uninstall  # removes Coucou from hooks.json
  python scripts/install-antigravity-hooks.py --preview    # prints diff without writing
"""

import sys
import os
import json
import difflib
from datetime import datetime

def get_target_path(is_local=False):
    if is_local:
        return os.path.abspath(os.path.join(".agents", "hooks.json"))
    home = os.path.expanduser("~")
    return os.path.join(home, ".gemini", "config", "hooks.json")

def get_hook_command_prefix():
    # If compiled coucou-hook.exe is in LocalAppData, prefer it
    local_app_data = os.environ.get("LOCALAPPDATA", "")
    coucou_exe = os.path.join(local_app_data, "Coucou", "bin", "coucou-hook.exe")
    if os.path.isfile(coucou_exe):
        exe_path = coucou_exe.replace("\\", "/")
        return f'"{exe_path}"'

    # Fallback to python relay script
    script_dir = os.path.dirname(os.path.abspath(__file__))
    relay_script = os.path.join(script_dir, "coucou-antigravity-hook.py").replace("\\", "/")
    return f'python "{relay_script}"'

def build_coucou_hooks(cmd_prefix):
    return {
        "PreInvocation": [
            {
                "type": "command",
                "command": f"{cmd_prefix} PreInvocation"
            }
        ],
        "PreToolUse": [
            {
                "matcher": "run_command",
                "hooks": [
                    {
                        "type": "command",
                        "command": f"{cmd_prefix} PermissionRequest",
                        "timeout": 120
                    }
                ]
            },
            {
                "matcher": "*",
                "hooks": [
                    {
                        "type": "command",
                        "command": f"{cmd_prefix} PreToolUse"
                    }
                ]
            }
        ],
        "PostToolUse": [
            {
                "matcher": "*",
                "hooks": [
                    {
                        "type": "command",
                        "command": f"{cmd_prefix} PostToolUse"
                    }
                ]
            }
        ],
        "Stop": [
            {
                "type": "command",
                "command": f"{cmd_prefix} Stop"
            }
        ]
    }

def main():
    args = sys.argv[1:]
    uninstall = "--uninstall" in args
    preview = "--preview" in args
    is_local = "--local" in args

    target = get_target_path(is_local)
    existing = {}

    if os.path.isfile(target):
        try:
            with open(target, "r", encoding="utf-8") as f:
                existing = json.load(f)
        except Exception as e:
            print(f"Error reading existing {target}: {e}", file=sys.stderr)
            sys.exit(1)

    after = dict(existing)
    cmd_prefix = get_hook_command_prefix()

    if uninstall:
        if "coucou" in after:
            del after["coucou"]
    else:
        after["coucou"] = build_coucou_hooks(cmd_prefix)

    before_str = json.dumps(existing, indent=2) + "\n"
    after_str = json.dumps(after, indent=2) + "\n"

    diff = list(difflib.unified_diff(
        before_str.splitlines(keepends=True),
        after_str.splitlines(keepends=True),
        fromfile=target + " (current)",
        tofile=target + " (updated)"
    ))

    if not diff:
        print("No changes required. Coucou hooks already up-to-date in:")
        print(f"  {target}")
        sys.exit(0)

    print("Diff to apply:")
    sys.stdout.writelines(diff)

    if preview:
        print("\nPreview mode — nothing written.")
        sys.exit(0)

    # Backup if file exists
    if os.path.isfile(target):
        stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        backup = target + f".bak-{stamp}"
        with open(backup, "w", encoding="utf-8") as f:
            f.write(before_str)
        print(f"\nBackup saved to: {backup}")

    os.makedirs(os.path.dirname(target), exist_ok=True)
    with open(target, "w", encoding="utf-8") as f:
        f.write(after_str)

    if uninstall:
        print(f"\nCoucou hooks uninstalled successfully from: {target}")
    else:
        print(f"\nCoucou hooks installed successfully in: {target}")
        print("Antigravity CLI will automatically invoke Coucou on your next session!")

if __name__ == "__main__":
    main()
