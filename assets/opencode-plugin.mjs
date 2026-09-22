// ordain-opencode-plugin: managed by `ordain integration install opencode`; remove with `ordain integration uninstall opencode`.
import { spawn } from "node:child_process";
import path from "node:path";

const ORDAIN = __ORDAIN_BINARY__;

const runHook = (name, payload, timeoutMs) => new Promise((resolve) => {
  let settled = false;
  const finish = (value) => { if (!settled) { settled = true; resolve(value); } };
  let child;
  try { child = spawn(ORDAIN, ["__hook", name], { stdio: ["pipe", "pipe", "ignore"] }); }
  catch { finish(undefined); return; }
  const chunks = [];
  const timer = setTimeout(() => { try { child.kill("SIGKILL"); } catch {} finish(undefined); }, timeoutMs);
  child.stdin.on("error", () => {});
  child.stdout.on("error", () => {});
  child.stdout.on("data", (chunk) => chunks.push(chunk));
  child.on("error", () => { clearTimeout(timer); finish(undefined); });
  child.on("close", () => {
    clearTimeout(timer);
    const text = Buffer.concat(chunks).toString("utf8").trim();
    if (!text) { finish(undefined); return; }
    try { finish(JSON.parse(text)); } catch { finish(undefined); }
  });
  try { child.stdin.end(JSON.stringify(payload)); } catch { clearTimeout(timer); finish(undefined); }
});

const hunksFrom = (diff) => {
  if (typeof diff !== "string") return undefined;
  const hunks = []; let current;
  for (const line of diff.split("\n")) {
    const header = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(line);
    if (header) {
      current = { oldStart: Number(header[1]), oldLines: header[2] === undefined ? 1 : Number(header[2]), newStart: Number(header[3]), newLines: header[4] === undefined ? 1 : Number(header[4]), lines: [] };
      hunks.push(current); continue;
    }
    if (current && /^[ +-]/.test(line)) current.lines.push(line);
  }
  return hunks.length ? hunks : undefined;
};

const textOf = (parts) => (Array.isArray(parts) ? parts : []).filter((part) => part && part.type === "text" && typeof part.text === "string").map((part) => part.text).join("\n");

export default async ({ client, directory }) => {
  const log = (message) => { try { Promise.resolve(client?.app?.log?.({ body: { service: "ordain", level: "info", message } })).catch(() => {}); } catch {} };
  const sessions = new Map();
  const absolute = (file) => typeof file !== "string" ? "" : path.isAbsolute(file) ? file : path.join(directory, file);
  return {
    "chat.message": async (input, output) => { try {
      const sessionID = input?.sessionID; const message = output?.message;
      if (!sessionID || !message?.id) return;
      const text = textOf(output.parts); let state = sessions.get(sessionID);
      if (!state) {
        state = { turnId: undefined, followups: 0, repairing: false }; sessions.set(sessionID, state);
        const result = await runHook("session-start", { session_id: sessionID, cwd: directory, hook_event_name: "SessionStart", source: "startup" }, 10000);
        const context = result?.hookSpecificOutput?.additionalContext;
        if (typeof context === "string" && context) output.parts.push({ id: `prt_ordain_${Date.now().toString(36)}`, sessionID, messageID: message.id, type: "text", text: context, synthetic: true });
        if (result?.systemMessage) log(result.systemMessage);
      }
      if (state.repairing && text.startsWith("Ordain:")) { state.repairing = false; return; }
      state.turnId = message.id; state.followups = 0;
      await runHook("turn-start", { session_id: sessionID, prompt_id: message.id, cwd: directory, hook_event_name: "UserPromptSubmit", prompt: text }, 10000);
    } catch {} },
    "tool.execute.after": async (input, output) => { try {
      if (input?.tool !== "edit" && input?.tool !== "write") return;
      const state = sessions.get(input.sessionID); if (!state) return;
      const args = input.args ?? {}; const file_path = absolute(args.filePath); if (!file_path) return;
      const hunks = hunksFrom(output?.metadata?.diff);
      const payload = input.tool === "edit"
        ? { tool_name: "Edit", tool_input: { file_path, old_string: String(args.oldString ?? ""), new_string: String(args.newString ?? ""), replace_all: Boolean(args.replaceAll) }, tool_response: hunks ? { filePath: file_path, structuredPatch: hunks } : {} }
        : { tool_name: "Write", tool_input: { file_path, content: String(args.content ?? "") }, tool_response: hunks ? { filePath: file_path, originalFile: "", structuredPatch: hunks } : { filePath: file_path, originalFile: null } };
      const result = await runHook("post-tool-use", { ...payload, session_id: input.sessionID, prompt_id: state.turnId, cwd: directory, hook_event_name: "PostToolUse", tool_use_id: input.callID }, 20000);
      if (result?.decision === "block" && typeof result.reason === "string") output.output = `${output.output ?? ""}\n\n${result.reason}`;
      const context = result?.hookSpecificOutput?.additionalContext;
      if (typeof context === "string") output.output = `${output.output ?? ""}\n\n${context}`;
      if (result?.systemMessage) log(result.systemMessage);
    } catch {} },
    event: async ({ event }) => { try {
      if (event?.type !== "session.idle") return;
      const sessionID = event.properties?.sessionID; const state = sessions.get(sessionID); if (!state?.turnId) return;
      const result = await runHook("stop", { session_id: sessionID, prompt_id: state.turnId, cwd: directory, hook_event_name: "Stop", stop_hook_active: state.followups > 0 }, 30000);
      if (result?.systemMessage) log(result.systemMessage);
      if (result?.decision === "block" && typeof result.reason === "string" && state.followups < 1) {
        state.followups += 1; state.repairing = true;
        await client.session.promptAsync({ path: { id: sessionID }, body: { parts: [{ type: "text", text: result.reason }] } });
      }
    } catch {} },
  };
};
