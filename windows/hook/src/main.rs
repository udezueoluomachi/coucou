//! coucou-hook — the relay Claude Code runs on every hook event.
//!
//! Reads the hook JSON on stdin, adds a little terminal context, and hands it to
//! Coucou over the named pipe `\\.\pipe\coucou-<sid>`.
//!
//! Hard rule (docs/CLAUDE.md): **never block Claude Code.**
//! * If the pipe does not exist — Coucou is closed — we exit 0 immediately with
//!   nothing on stdout, and the session carries on untouched.
//! * Every step runs under a deadline enforced by the main thread, so a pipe that
//!   accepts the connection and then stops reading cannot wedge the session
//!   either: we abandon the worker and exit.
//! * Only `PermissionRequest` waits for an answer, because approving from the
//!   island is the whole point. No answer means empty stdout, and Claude Code
//!   asks in the terminal exactly as if Coucou were not installed.
//!
//! Usage: `coucou-hook <EventName>` (the name is also read from the JSON).

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Budget for getting a pipe connection. Beyond this Claude Code wins, always.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
/// Whole-run budget for an event nobody waits on: connect and write, no more.
const FIRE_AND_FORGET_BUDGET: Duration = Duration::from_secs(2);
/// How long a permission prompt may stay on screen before the terminal takes over.
const DECISION_BUDGET: Duration = Duration::from_secs(110);

/// `ERROR_PIPE_BUSY` — every instance is serving someone else right now. This is
/// the one error worth retrying: the server exists and a slot will free up.
const ERROR_PIPE_BUSY: i32 = 231;

/// Fields that are pointless to forward and can be enormous (a whole file read,
/// a full command output). The island never shows them.
const DROPPED_FIELDS: &[&str] = &["tool_response", "transcript_path"];
/// Longest string forwarded for any single field; the island truncates to far
/// less than this anyway.
const MAX_FIELD_LEN: usize = 2_000;

mod win;

/// `\\.\pipe\coucou-<sid>`. The SID keeps two accounts on the same machine from
/// ever meeting on the same pipe; the name falls back to the user name only if
/// the SID cannot be read at all, which should not happen.
fn pipe_path() -> String {
    let key = win::current_user_sid()
        .unwrap_or_else(|| std::env::var("USERNAME").unwrap_or_else(|_| "user".into()));
    format!(r"\\.\pipe\coucou-{key}")
}

/// Opens the pipe. Retries only while the server is busy: any other error means
/// there is nothing to talk to, and waiting would only delay Claude Code.
fn connect() -> Option<std::fs::File> {
    use std::os::windows::io::AsRawHandle;
    let path = pipe_path();
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => {
                let handle = windows::Win32::Foundation::HANDLE(file.as_raw_handle());
                // Somebody else's server on our pipe name gets nothing from us.
                return win::pipe_server_is_same_user(handle).then_some(file);
            }
            Err(err) => {
                if err.raw_os_error() != Some(ERROR_PIPE_BUSY) || Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    }
}

fn main() {
    let Some((payload, event, is_antigravity, tool_name, tool_input)) = read_event() else { std::process::exit(0) };

    let waits_for_answer = event == "PermissionRequest";
    let budget = if waits_for_answer { DECISION_BUDGET } else { FIRE_AND_FORGET_BUDGET };

    // The worker owns every blocking call. If it overruns the budget we simply
    // stop listening and exit: the process dying takes the pipe handle with it.
    // (No catch_unwind here — the release profile is panic = "abort", so it would
    // be dead code. `talk` is written to have nothing to panic on instead.)
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let _ = tx.send(talk(&payload, waits_for_answer));
    });

    if let Ok(Some(decision)) = rx.recv_timeout(budget) {
        if let Some(json) = decision_json(&decision, is_antigravity, &tool_name, &tool_input) {
            let mut out = std::io::stdout();
            let _ = writeln!(out, "{json}");
            let _ = out.flush();
        }
    }
    // Nothing printed: asks in the terminal, as if we were not here.
    std::process::exit(0);
}

/// The documented PermissionRequest output for Claude Code or Antigravity CLI.
/// Anything we do not recognise prints nothing at all rather than guessing.
fn decision_json(
    decision: &str,
    is_antigravity: bool,
    tool_name: &str,
    tool_input: &serde_json::Value,
) -> Option<String> {
    if is_antigravity {
        match decision.trim() {
            "allow" | "always" => {
                let mut overrides = Vec::new();
                if tool_name == "run_command" {
                    if let Some(cmd) = tool_input.get("CommandLine").or_else(|| tool_input.get("command")).and_then(|v| v.as_str()) {
                        overrides.push(format!("command({cmd})"));
                    }
                    overrides.push("command(*)".to_string());
                } else if tool_name == "write_to_file" || tool_name == "replace_file_content" {
                    if let Some(target) = tool_input.get("TargetFile").or_else(|| tool_input.get("path")).and_then(|v| v.as_str()) {
                        overrides.push(format!("file({target})"));
                    }
                    overrides.push("file(*)".to_string());
                } else {
                    overrides.push("command(*)".to_string());
                    overrides.push("file(*)".to_string());
                }
                let val = serde_json::json!({
                    "decision": "allow",
                    "permissionOverrides": overrides,
                    "allowTool": true,
                });
                Some(val.to_string())
            }
            "deny" => Some(r#"{"decision":"deny","reason":"Denied from Coucou"}"#.to_string()),
            _ => None,
        }
    } else {
        let behavior = match decision.trim() {
            // "always" still answers a plain allow; remembering it is the island's
            // business, not Claude Code's.
            "allow" | "always" => r#"{"behavior":"allow"}"#.to_string(),
            "deny" => r#"{"behavior":"deny","message":"Denied from Coucou"}"#.to_string(),
            _ => return None,
        };
        Some(format!(
            r#"{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{behavior}}}}}"#
        ))
    }
}

/// Reads stdin and returns the payload to forward plus the event name, whether it's Antigravity, tool name and input.
fn read_event() -> Option<(String, String, bool, String, serde_json::Value)> {
    let mut raw = Vec::new();
    if std::io::stdin().read_to_end(&mut raw).is_err() || raw.is_empty() {
        return None;
    }
    // Some shells hand us a UTF-8 BOM; serde_json would choke on it.
    if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        raw.drain(..3);
    }

    let mut payload = serde_json::from_slice::<serde_json::Value>(&raw).ok()?;
    let map = payload.as_object_mut()?;

    let args: Vec<String> = std::env::args().collect();
    let arg_event = args.get(1).cloned().unwrap_or_default();
    let has_ask_flag = args.iter().any(|a| a == "--ask" || a == "--approve");

    // Detect Antigravity CLI payload:
    // Antigravity passes camelCase fields: conversationId, toolCall, workspacePaths, transcriptPath, stepIdx
    let is_antigravity = map.contains_key("conversationId") || map.contains_key("toolCall");

    let event: String;
    if is_antigravity {
        map.insert("source".into(), serde_json::Value::String("antigravity".into()));

        // Map session_id from conversationId
        if let Some(conv_id) = map.get("conversationId").and_then(|v| v.as_str()) {
            map.insert("session_id".into(), serde_json::Value::String(conv_id.to_string()));
        }

        // Map cwd from workspacePaths if available
        if !map.contains_key("cwd") {
            if let Some(ws) = map
                .get("workspacePaths")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
            {
                map.insert("cwd".into(), serde_json::Value::String(ws.to_string()));
            }
        }

        // Map toolCall if present
        if let Some(tool_call) = map.get("toolCall").and_then(|v| v.as_object()).cloned() {
            if let Some(name) = tool_call.get("name").and_then(|v| v.as_str()) {
                map.insert("tool_name".into(), serde_json::Value::String(name.to_string()));
            }
            if let Some(tool_args) = tool_call.get("args") {
                map.insert("tool_input".into(), tool_args.clone());
            }
        }

        // Map event name
        if arg_event == "PermissionRequest" || has_ask_flag {
            event = "PermissionRequest".to_string();
        } else if arg_event == "PreInvocation" {
            event = "UserPromptSubmit".to_string();
        } else if arg_event == "PreToolUse" {
            event = "PreToolUse".to_string();
        } else if arg_event == "PostToolUse" {
            let has_err = map.get("error").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
            event = if has_err { "PostToolUseFailure".to_string() } else { "PostToolUse".to_string() };
        } else if arg_event == "Stop" {
            let has_err = map.get("error").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
            event = if has_err { "StopFailure".to_string() } else { "Stop".to_string() };
        } else if !arg_event.is_empty() {
            event = arg_event;
        } else {
            event = map.get("hook_event_name").and_then(|v| v.as_str()).unwrap_or("PreToolUse").to_string();
        }
    } else {
        // The event name is passed as argv[1] by the hook command; the JSON usually
        // carries it too. Trust argv when the JSON is missing it.
        event = map
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or(arg_event);
    }
    map.insert("hook_event_name".into(), serde_json::Value::String(event.clone()));

    for field in DROPPED_FIELDS {
        map.remove(*field);
    }

    let cwd_missing = map
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(str::is_empty)
        .unwrap_or(true);
    if cwd_missing {
        if let Ok(cwd) = std::env::current_dir() {
            map.insert(
                "cwd".into(),
                serde_json::Value::String(cwd.to_string_lossy().to_string()),
            );
        }
    }

    // Which terminal the session runs in. Unlike macOS, Coucou on Windows accepts
    // events from every terminal, so this is context only — never a filter.
    for (key, var) in [
        ("term_program", "TERM_PROGRAM"),
        ("wt_session", "WT_SESSION"),
        ("term_session_id", "TERM_SESSION_ID"),
        ("vscode_pid", "VSCODE_PID"),
        ("session_pid", "CLAUDE_CODE_SSE_PORT"),
    ] {
        if !map.contains_key(key) {
            let value = std::env::var(var).unwrap_or_default();
            map.insert(key.into(), serde_json::Value::String(value));
        }
    }

    let tool_name = map.get("tool_name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let tool_input = map.get("tool_input").cloned().unwrap_or(serde_json::Value::Null);

    truncate_strings(&mut payload);

    let mut line = payload.to_string();
    line.push('\n');
    Some((line, event, is_antigravity, tool_name, tool_input))
}

/// Caps every string in the payload. A single Write can carry a whole file.
fn truncate_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if s.len() > MAX_FIELD_LEN {
                // Cut on a char boundary; a lone byte index can split UTF-8.
                let mut end = MAX_FIELD_LEN;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                s.push('…');
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(truncate_strings),
        serde_json::Value::Object(map) => map.values_mut().for_each(truncate_strings),
        _ => {}
    }
}

/// Connect, send, and — for a permission request — wait for the island's word.
fn talk(payload: &str, waits_for_answer: bool) -> Option<String> {
    let mut pipe = connect()?;

    if pipe.write_all(payload.as_bytes()).is_err() {
        return None;
    }
    let _ = pipe.flush();

    if !waits_for_answer {
        return None;
    }

    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let answer = String::from_utf8_lossy(&buf).trim().to_string();
    (!answer.is_empty()).then_some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_json_matches_the_documented_shape() {
        let dummy = serde_json::json!({});
        assert_eq!(
            decision_json("allow", false, "", &dummy).unwrap(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        );
        assert_eq!(
            decision_json("deny", false, "", &dummy).unwrap(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"Denied from Coucou"}}}"#
        );
        let agy_allow = decision_json("allow", true, "run_command", &serde_json::json!({"CommandLine": "dir"})).unwrap();
        assert!(agy_allow.contains(r#""decision":"allow""#));
        assert!(agy_allow.contains(r#""permissionOverrides":["command(dir)","command(*)"]"#));
        assert!(agy_allow.contains(r#""allowTool":true"#));
        assert_eq!(
            decision_json("deny", true, "", &dummy).unwrap(),
            r#"{"decision":"deny","reason":"Denied from Coucou"}"#
        );
        // "always" is an island concept; Claude Code just gets an allow.
        assert!(decision_json("always", false, "", &dummy).unwrap().contains(r#""behavior":"allow""#));
        assert!(decision_json("always", true, "", &dummy).unwrap().contains(r#""decision":"allow""#));
    }

    #[test]
    fn anything_unrecognised_prints_nothing() {
        let dummy = serde_json::json!({});
        assert!(decision_json("", false, "", &dummy).is_none());
        assert!(decision_json("maybe", false, "", &dummy).is_none());
        assert!(decision_json("", true, "", &dummy).is_none());
        assert!(decision_json("maybe", true, "", &dummy).is_none());
        // The shape the app used to send must not be mistaken for a decision.
        assert!(decision_json(r#"{"permissionDecision":"allow"}"#, false, "", &dummy).is_none());
    }

    #[test]
    fn long_strings_are_cut_on_a_char_boundary() {
        let mut v = serde_json::json!({ "tool_input": { "content": "é".repeat(4000) } });
        truncate_strings(&mut v);
        let s = v["tool_input"]["content"].as_str().unwrap();
        assert!(s.len() <= MAX_FIELD_LEN + 4);
        assert!(s.ends_with('…'));
    }
}
