# Coucou — guide for Google Antigravity & Gemini agents

Coucou is a desktop companion app for AI coding agents: Mochi, a small animated character living at the top of your screen, tracks Antigravity CLI and Claude Code sessions, displays integrations, and lets you approve tool calls, chat, and drop files directly from the notch/status bar.

## Where things are
- windows/src-tauri/ — Rust backend (Tauri v2, named pipe server, Windows Credential Manager, Win32 window management, Google Gemini & Claude REST APIs).
- windows/src/ — Webview frontend (TypeScript, DOM views, Mochi canvas engine, layout, settings).
- windows/hook/ — Standalone native relay (coucou-hook.exe).
- scripts/ — Hook relays (coucou-antigravity-hook.py, installer scripts).
- NotchBuddy/Sources/App/ — macOS Swift application.
- docs/ — Specifications and documentation.

## Build (Windows)
`powershell
cd windows
npm install
npm run build
cargo build --release -p coucou -p coucou-hook
`
The compiled application lives at windows/target/release/coucou.exe and the hook relay at windows/target/release/coucou-hook.exe.

## Rules
- **Secrets live in Windows Credential Manager**: API keys (gemini-api-key, nthropic-api-key) and integration tokens are never written to disk or sent across the webview IPC boundary.
- **Dedicated model selectors**: Google Gemini supports Gemini 3.0+ models (gemini-3.1-pro, gemini-3.0-pro, gemini-3.0-flash). Anthropic Claude models remain separate and unmodified.
- **Antigravity lifecycle hooks**: Configured in ~/.gemini/config/hooks.json. Must remain non-blocking: if Coucou is closed or times out, hooks return {decision: ask} so the terminal takes over.
- **Seamless permissions**: When a tool approval is granted on the Coucou notch, the hook supplies permissionOverrides so Antigravity CLI executes the action without secondary terminal prompts.
- **Terminal window activation**: Clicking an active Antigravity session or Open terminal restores and focuses the native Win32 terminal window (EnumWindows, SetForegroundWindow), never forcing external editors like VS Code.
- **No telemetry**: Network calls only go directly to services configured by the user.
