// Claude Code hook events → island state.
// Port of HookServer.processEvent / processPermissionRequest from the macOS app.
// Difference from macOS: no terminal filter. On Windows the hook fires from any
// terminal (Windows Terminal, VS Code, PowerShell…) and all of them are handled.

import { Bridge, onEvent } from "../core/bridge";
import { Sound } from "../core/sound";
import { State } from "../core/state";
import type { Island } from "./island";

const CLAUDE_ID = "integration_claude";

/** Clears the approval card if no decision was made before the hook gave up. */
let pendingTimeout: number | null = null;

interface HookPayload {
  hook_event_name?: string;
  request_id?: string;
  session_id?: string;
  cwd?: string;
  message?: string;
  /** UserPromptSubmit carries `prompt`; `message` belongs to Notification/Stop. */
  prompt?: string;
  tool_name?: string;
  tool_input?: Record<string, unknown>;
  source?: string;
  terminal_pids?: number[];
}

const PROJECT_ALIASES: Record<string, string> = {
  "notch-buddy": "Notch Buddy",
  notchbuddy: "Notch Buddy",
  notch_buddy: "Notch Buddy",
};

function aliasProjectName(name: string): string {
  return PROJECT_ALIASES[name.toLowerCase()] ?? name;
}

function lastPathComponent(p: string): string {
  const cleaned = p.replace(/[\\/]+$/, "");
  const idx = Math.max(cleaned.lastIndexOf("\\"), cleaned.lastIndexOf("/"));
  return idx >= 0 ? cleaned.slice(idx + 1) : cleaned;
}

/** frenchStep() — tool labels for Claude Code and Antigravity CLI. */
const TOOL_LABELS: Record<string, string> = {
  // Claude Code tools
  Bash: "Exécute",
  Read: "Lit",
  Write: "Écrit",
  Edit: "Modifie",
  Glob: "Cherche",
  Grep: "Recherche",
  WebSearch: "Recherche web",
  WebFetch: "Récupère",
  TodoWrite: "Tâches",
  Task: "Agent",
  LS: "Liste",
  MultiEdit: "Modifie",
  NotebookEdit: "Notebook",
  PowerShell: "Exécute",

  // Antigravity CLI tools
  run_command: "Exécute",
  view_file: "Lit",
  replace_file_content: "Modifie",
  write_to_file: "Écrit",
  grep_search: "Recherche",
  find_by_name: "Cherche",
  search_web: "Recherche web",
  read_url_content: "Récupère",
  call_mcp_tool: "MCP",
  generate_image: "Génère",
  ask_question: "Question",
  schedule: "Planifie",
  manage_task: "Tâche",
  invoke_subagent: "Sous-agent",
  send_message: "Message",
  list_dir: "Liste",
};

function stepLabel(tool: string, input: Record<string, unknown>): string {
  const label = TOOL_LABELS[tool] ?? tool;
  const str = (k: string) => (typeof input[k] === "string" ? (input[k] as string) : null);

  // Command execution
  const cmd = str("command") ?? str("CommandLine");
  if (cmd) return `${label} · ${cmd.slice(0, 40)}`;

  // File paths & locations
  const path = str("path") ?? str("AbsolutePath") ?? str("DirectoryPath");
  if (path) return `${label} · ${lastPathComponent(path)}`;
  const file = str("file_path") ?? str("TargetFile");
  if (file) return `${label} · ${lastPathComponent(file)}`;

  // Searches & queries
  const query = str("query") ?? str("Query") ?? str("Pattern");
  if (query) return `${label} · ${query.slice(0, 40)}`;

  // URLs & MCP
  const url = str("url") ?? str("Url");
  if (url) return `${label} · ${url.slice(0, 40)}`;
  const mcpTool = str("ToolName");
  if (mcpTool) return `${label} · ${mcpTool}`;

  // Subagents
  const subagent = str("Role") ?? str("TypeName");
  if (subagent) return `${label} · ${subagent}`;

  return label;
}

/**
 * What the Allow button actually authorises.
 */
const APPROVAL_FIELDS = [
  "command", // Claude Code Bash, PowerShell
  "CommandLine", // Antigravity run_command
  "file_path", // Claude Code Write, Edit
  "TargetFile", // Antigravity replace_file_content, write_to_file
  "path", // Claude Code Read, LS
  "AbsolutePath", // Antigravity view_file
  "DirectoryPath", // Antigravity list_dir
  "url", // Claude Code WebFetch
  "Url", // Antigravity read_url_content
  "query", // Claude Code WebSearch
  "Query", // Antigravity grep_search
  "pattern", // Claude Code Glob, Grep
  "Pattern", // Antigravity find_by_name
  "ToolName", // Antigravity call_mcp_tool
  "prompt", // Claude Code Task
  "Prompt", // Antigravity schedule, generate_image
] as const;

function approvalTarget(tool: string, input: Record<string, unknown>): string {
  for (const field of APPROVAL_FIELDS) {
    const value = input[field];
    if (typeof value === "string" && value.trim()) {
      return `${tool} · ${value.trim()}`;
    }
  }
  return tool;
}

function upsert(projectName: string, cwd: string, source?: string, terminalPids?: number[]) {
  const t = State.tasks.find((x) => x.id === CLAUDE_ID);
  if (!t) return;
  t.name = projectName;
  if (cwd) t.sessionCwd = cwd;
  if (source === "antigravity") {
    t.source = "antigravity";
  }
  if (terminalPids && terminalPids.length > 0) {
    t.terminalPids = terminalPids;
  }
}

function clearSession() {
  const t = State.tasks.find((x) => x.id === CLAUDE_ID);
  if (!t) return;
  t.steps = [];
  t.stepIndex = 0;
  t.name = t.source === "antigravity" ? "Antigravity" : "VS Code";
  t.pillBadge = null;
}

export function registerHookHandlers(island: Island) {
  void onEvent<HookPayload>("hook", (payload) => handleHook(island, payload));
}

function handleHook(island: Island, payload: HookPayload) {
  if (State.paused) {
    // Silence here used to cost Claude Code nearly two minutes: the relay waited
    // for a decision from an island that had already decided not to look. Say so,
    // and the terminal takes the question immediately.
    if (payload.request_id) void Bridge.approvalDecline(payload.request_id);
    return;
  }

  const name = payload.hook_event_name ?? "";
  const cwd = payload.cwd ?? "";
  const raw = lastPathComponent(cwd);
  const projectName = aliasProjectName(raw || "Session");
  const focused = State.focusId === CLAUDE_ID;

  /** Alerts force the island open; work events only reveal the compact island. */
  const surface = (view: Parameters<Island["alert"]>[0], isAlert: boolean) => {
    if (State.mode === "expanded") {
      if (isAlert) island.setView(view);
    } else if (isAlert) {
      island.alert(view);
    } else if (State.mode === "hidden") {
      island.reveal();
    }
  };

  switch (name) {
    case "SessionStart":
      upsert(projectName, cwd, payload.source, payload.terminal_pids);
      surface("overview", false);
      Sound.play("work");
      break;

    case "UserPromptSubmit": {
      upsert(projectName, cwd, payload.source, payload.terminal_pids);
      State.updateTask(CLAUDE_ID, "thinking");
      // The field is `prompt`; reading `message` meant this step was always blank.
      const asked = payload.prompt ?? payload.message;
      if (asked) State.appendStep(CLAUDE_ID, asked.slice(0, 60));
      surface("overview", false);
      break;
    }

    case "PreToolUse": {
      upsert(projectName, cwd, payload.source, payload.terminal_pids);
      State.updateTask(CLAUDE_ID, "working");
      const tool = payload.tool_name ?? "Tool";
      State.appendStep(CLAUDE_ID, stepLabel(tool, payload.tool_input ?? {}));
      surface("overview", false);
      break;
    }

    case "PostToolUse":
      State.updateTask(CLAUDE_ID, "working");
      break;

    case "PostToolUseFailure":
      State.updateTask(CLAUDE_ID, "working");
      State.appendStep(CLAUDE_ID, "⚠ failed");
      break;

    case "Notification": {
      const message = payload.message ?? "";
      const lower = message.toLowerCase();
      if (lower.includes("rate limit") || lower.includes("limite d")) {
        State.updateTask(CLAUDE_ID, "ratelimit");
        Sound.play("rate");
      } else if (message.endsWith("?")) {
        State.updateTask(CLAUDE_ID, "question");
        State.appendStep(CLAUDE_ID, message);
      }
      break;
    }

    case "Stop":
      State.updateTask(CLAUDE_ID, "finished");
      if (payload.message) State.appendStep(CLAUDE_ID, payload.message.slice(0, 60));
      Sound.play("finish");
      if (focused) surface("finished", true);
      else State.setPillBadge(CLAUDE_ID, "finished");
      window.setTimeout(() => {
        State.updateTask(CLAUDE_ID, "idle");
        State.setPillBadge(CLAUDE_ID, null);
      }, 5200);
      break;

    case "StopFailure":
      State.updateTask(CLAUDE_ID, "error");
      Sound.play("error");
      if (focused) surface("error", true);
      else State.setPillBadge(CLAUDE_ID, "error");
      break;

    case "SessionEnd":
      State.updateTask(CLAUDE_ID, "idle");
      clearSession();
      break;

    case "SubagentStart":
      State.appendStep(CLAUDE_ID, "+ subagent");
      break;

    case "SubagentStop":
      State.appendStep(CLAUDE_ID, "• subagent done");
      break;

    case "PermissionRequest": {
      const requestId = payload.request_id ?? "";
      // One card, one request. A second one must never quietly replace the first
      // — that would leave a human staring at request B while request A waits for
      // a decision nobody can give. Hand it straight back to the terminal.
      if (State.pendingApproval && State.pendingApproval.requestId !== requestId) {
        if (requestId) void Bridge.approvalDecline(requestId);
        break;
      }
      upsert(projectName, cwd, payload.source);
      if (pendingTimeout != null) window.clearTimeout(pendingTimeout);
      const tool = payload.tool_name ?? "Tool";
      const input = payload.tool_input ?? {};
      State.pendingApproval = {
        requestId,
        sessionId: payload.session_id ?? "",
        tool,
        command: approvalTarget(tool, input),
      };
      // The relay's short ack window closes in 800 ms; everything below this
      // line is synchronous, so the card really is up by the time it lands.
      if (requestId) void Bridge.approvalAck(requestId);
      State.updateTask(CLAUDE_ID, "approval");
      State.isPinned = true;
      Sound.play("approval");
      if (focused) {
        island.alert("approval");
      } else {
        // Another agent holds the view, so the card would yank it away. The badge
        // is the signal instead — but it has to be on screen for that to mean
        // anything, hence the reveal. We just told the relay a human can act.
        State.setPillBadge(CLAUDE_ID, "approval");
        island.reveal();
      }
      // Coucou answers within 108 s or not at all; after that the terminal has
      // taken over and the card would be lying.
      pendingTimeout = window.setTimeout(() => {
        pendingTimeout = null;
        if (!State.pendingApproval) return;
        State.pendingApproval = null;
        State.isPinned = false;
        island.dropPin();
        State.updateTask(CLAUDE_ID, "working");
        State.setPillBadge(CLAUDE_ID, null);
        if (State.view === "approval") island.setView(State.defaultView());
        State.notify();
      }, 110_000);
      break;
    }

    default:
      break;
  }
  State.notify();
}
