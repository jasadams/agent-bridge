/** Installed-asset OpenCode V1 acceptance with an isolated scripted local model.
 * No owner credentials/configuration or external model inference are used. */
import { createOpencodeClient } from "@opencode-ai/sdk";
import { chmod, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { randomUUID } from "node:crypto";
import { brokerRpc } from "./rpc.ts";

const binary = process.argv[2] && resolve(process.argv[2]);
if (!binary) throw new Error("Usage: bun setup-native.ts /absolute/built/agent-bridge [opencode]");
const opencode = Bun.which(process.argv[3] ?? "opencode");
if (!opencode) throw new Error("OpenCode executable unavailable");
const root = await realpath(await mkdtemp("/tmp/aboc-setup-"));
await chmod(root, 0o700);
const home = join(root, "home"), workspace = join(root, "workspace"), state = join(root, "state");
for (const path of [home, workspace, state]) await mkdir(path, { mode: 0o700 });
const env = {
  PATH: process.env.PATH ?? "/usr/bin:/bin", HOME: home,
  XDG_CONFIG_HOME: join(home, ".config"), XDG_DATA_HOME: join(home, ".local/share"),
  XDG_CACHE_HOME: join(home, ".cache"), XDG_STATE_HOME: join(home, ".local/state"),
  OPENCODE_DISABLE_DEFAULT_PLUGINS: "true", OPENCODE_DISABLE_MODELS_FETCH: "true",
  OPENCODE_DISABLE_AUTOUPDATE: "true", OPENCODE_DISABLE_SHARE: "true", OPENCODE_DISABLE_MCP_OAUTH: "true", DO_NOT_TRACK: "1",
};
function assert(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
async function waitFor<T>(label: string, inspect: () => Promise<T | false | undefined>, milliseconds = 30000): Promise<T> {
  const deadline = Date.now() + milliseconds; let last: unknown;
  do { try { const value = await inspect(); if (value) return value; } catch (error) { last = error; } await Bun.sleep(100); } while (Date.now() < deadline);
  throw new Error(`Timed out: ${label}${last ? ` (${last})` : ""}`);
}
async function stop(child: ReturnType<typeof Bun.spawn>) {
  if (child.exitCode !== null) { await child.exited; return; }
  child.kill("SIGTERM"); const timer = setTimeout(() => child.kill("SIGKILL"), 5000);
  try { await child.exited; } finally { clearTimeout(timer); }
}
async function command(argv: string[], reject = false) {
  const child = Bun.spawn(argv, { cwd: workspace, env, stdout: "pipe", stderr: "pipe" });
  const output = Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text()]);
  const timer = setTimeout(() => child.kill("SIGKILL"), 30000);
  try { const code = await child.exited, [out, err] = await output; assert(reject ? code !== 0 && (out + err).includes("unavailable") : code === 0, `${argv[1]} unexpected exit (${code}): ${out}\n${err}`); return out; }
  finally { clearTimeout(timer); }
}
const incoming = randomUUID(), replyID = randomUUID(), marker = `SETUP-PEER-${randomUUID()}`;
const ownBoard = "Disposable task | active | no blocker\n";
let session = "", selected = "", phase = "roundtrip", stage = 0, pairingDenied = false, discoveryVerified = false;
const captures: Array<{ phase: string; tools: number; systemBlocks: number; exactIdentity: boolean; compaction: boolean; transcriptSetup: boolean; tool?: string }> = [];
const modelErrors: string[] = [];
function readToolResult(body: any, id: string): any {
  const message = body.messages.findLast((m: any) => m.role === "tool" && m.tool_call_id === id);
  assert(message && typeof message.content === "string", `Missing actual tool result ${id}`);
  const data = JSON.parse(message.content);
  assert(!data.error, `Tool ${id} failed: ${message.content}`);
  return data.result ?? data;
}
const model = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
  try {
    const body = await request.json() as any, tools = body.tools ?? [];
    const systems = body.messages.filter((m: any) => m.role === "system").map((m: any) => typeof m.content === "string" ? m.content : JSON.stringify(m.content)).join("\n");
    const transcript = JSON.stringify(body.messages.filter((m: any) => m.role !== "system"));
    let call: any;
    const invoke = (suffix: string, args: unknown, id: string) => {
      const tool = tools.find((t: any) => t.function?.name.endsWith(suffix));
      assert(tool, `Missing installed MCP tool ${suffix}`);
      return { id, type: "function", function: { name: tool.function.name, arguments: JSON.stringify(args) } };
    };
    if (tools.length && selected) {
      if (phase === "roundtrip" && transcript.includes(marker)) {
        if (stage === 0) call = invoke("list_agents", { verbose: true }, "setup_discover");
        if (stage === 1) {
          const agents = readToolResult(body, "setup_discover");
          assert(Array.isArray(agents) && agents.some(a => a.name === selected && a.provider === "opencode" && a.session === session), "Actual MCP discovery omitted the exact installed native session");
          discoveryVerified = true;
          call = invoke("acknowledge_message", { id: incoming, agent: selected }, "setup_ack");
        }
        if (stage === 2) call = invoke("send_message", { from: selected, to: "native-probe", body: `Reply ${marker}`, id: replyID, parent: incoming }, "setup_reply");
        if (stage === 3) call = invoke("read_board", {}, "setup_read");
        if (stage === 4) call = invoke("subscribe_board", { agent: selected }, "setup_subscribe");
        if (stage === 5) call = invoke("update_board", { agent: selected, expected_revision: readToolResult(body, "setup_read").revision, edits: [{ op: "set", markdown: ownBoard }] }, "setup_update");
      } else if (phase === "pairing" && stage === 0) call = invoke("open_pairing", { agent: selected, seconds: 30 }, "setup_pairing_denied");
      else if (phase === "pairing" && stage === 1) {
        const result = body.messages.findLast((m: any) => m.role === "tool" && m.tool_call_id === "setup_pairing_denied");
        assert(result && JSON.stringify(result.content).includes("unavailable"), "MCP pairing must report unavailable owner input");
        pairingDenied = true;
      } else if (phase === "unsubscribe" && stage === 0) call = invoke("unsubscribe_board", { agent: selected }, "setup_unsubscribe");
      else if (phase === "subscribe" && stage === 0) call = invoke("subscribe_board", { agent: selected }, "setup_resubscribe");
      else if (phase === "notice" && stage === 0) call = invoke("read_board", {}, "setup_notice_read");
      if (call) stage++;
    }
    const systemBlocks = systems.split("<agent-bridge-session-context>").length - 1;
    const exactIdentity = selected !== "" && systems.includes(`identity is ${selected}`) && systems.includes(JSON.stringify(state));
    const compaction = transcript.includes("<agent-bridge-compaction-context>");
    captures.push({ phase, tools: tools.length, systemBlocks, exactIdentity, compaction, transcriptSetup: transcript.includes("<agent-bridge-session-context>"), tool: call?.function.name });
    // Tool-bearing turns must be the explicitly selected native session, never a
    // title/small-model request. Check every such actual provider request.
    if (tools.length) assert(systemBlocks === 1 && exactIdentity, "Installed exact-session system context missing or duplicated");
    assert(!transcript.includes("<agent-bridge-session-context>"), "Setup instructions accumulated in native transcript");
    const content = call ? null : compaction ? "Disposable session summary; peer references remain data." : "Disposable installed native fixture complete.";
    const response = { id: "chatcmpl-setup", object: "chat.completion", created: Math.floor(Date.now()/1000), model: "fixture", choices: [{ index: 0, message: { role: "assistant", content, ...(call ? { tool_calls: [call] } : {}) }, finish_reason: call ? "tool_calls" : "stop" }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } };
    if (!body.stream) return Response.json(response);
    const chunk = (delta: unknown, finish_reason: string | null) => `data: ${JSON.stringify({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta, finish_reason }] })}\n\n`;
    return new Response(chunk({ role: "assistant", ...(call ? { tool_calls: [{ index: 0, ...call }] } : { content }) }, null) + chunk({}, call ? "tool_calls" : "stop") + "data: [DONE]\n\n", { headers: { "content-type": "text/event-stream" } });
  } catch (error) { modelErrors.push(String(error)); return Response.json({ error: { message: String(error) } }, { status: 500 }); }
}});
const logs: Array<Promise<string>> = [], children: Array<ReturnType<typeof Bun.spawn>> = [];
let native: ReturnType<typeof Bun.spawn> | undefined, passed = false;
function start(argv: string[]) {
  const child = Bun.spawn(argv, { cwd: workspace, env, stdout: "pipe", stderr: "pipe" }); children.push(child);
  logs.push(new Response(child.stdout).text(), new Response(child.stderr).text()); return child;
}
const reserve = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response() });
const port = reserve.port!; reserve.stop(true);
try {
  assert((await command([opencode, "--version"])).trim() === "1.18.35", "Native harness requires exactly OpenCode 1.18.35");
  const configDir = join(env.XDG_CONFIG_HOME, "opencode"), configFile = join(configDir, "opencode.json");
  await mkdir(configDir, { recursive: true, mode: 0o700 });
  const fixtureConfig = {
    $schema: "https://opencode.ai/config.json", enabled_providers: ["fixture"], model: "fixture/fixture", small_model: "fixture/fixture",
    provider: { fixture: { npm: "@ai-sdk/openai-compatible", name: "Fixture", options: { baseURL: `http://127.0.0.1:${model.port}/v1`, apiKey: "local-fixture-only" }, models: { fixture: { name: "Fixture", limit: { context: 32768, output: 2048 }, tool_call: true } } } },
  };
  await writeFile(configFile, JSON.stringify(fixtureConfig), { mode: 0o600 });
  await command([binary, "--state-dir", state, "opencode", "install"]);
  const installed = JSON.parse(await readFile(configFile, "utf8"));
  const plugin = installed.plugin.find((p: any) => Array.isArray(p) && p[1]?.stateDir === state);
  assert(plugin && plugin[0].startsWith("file://"), "Installer did not configure an absolute local plugin asset");
  const asset = fileURLToPath(plugin[0]);
  assert(asset.startsWith(env.XDG_DATA_HOME + "/") && !asset.includes("/repos/"), "Native must load installed assets, not checkout source");
  assert(installed.mcp["agent-bridge"].command.includes(binary) && installed.mcp["agent-bridge"].command.includes(state), "Installer MCP does not pin binary and isolated state");
  start([binary, "--state-dir", state, "serve", "--no-discovery"]);
  await waitFor("isolated broker", () => brokerRpc(state, { op: "status" }));
  const startNative = () => start([opencode, "serve", "--hostname", "127.0.0.1", "--port", String(port), "--print-logs", "--log-level", "DEBUG"]);
  native = startNative();
  const baseUrl = `http://127.0.0.1:${port}`;
  const healthy = () => waitFor("native HTTP health", async () => (await fetch(baseUrl + "/global/health", { signal: AbortSignal.timeout(2000) })).ok);
  await healthy();
  const client = createOpencodeClient({ baseUrl, directory: workspace, fetch: request => fetch(request, { signal: AbortSignal.timeout(60000) }) });
  const created = await client.session.create({ body: { title: "Installed native setup acceptance" } });
  assert(created.data && !created.error, "Cannot create isolated native session"); session = created.data.id; selected = `opencode-${session}`;
  const registration = () => waitFor<any>("installed live registration", async () => (await brokerRpc(state, { op: "list", all: false }) as any[]).find(a => a.name === selected));
  await registration();
  const messages = async () => { const result = await client.session.messages({ path: { id: session } }); assert(result.data, "Cannot read native transcript"); return result.data; };
  const notices = async () => (await messages()).filter(m => m.info.role === "user" && m.parts.some(p => p.type === "text" && p.text.includes("local board changed")));
  const idle = () => waitFor("native idle", async () => { const r = await client.session.status(); return !!r.data && (!r.data[session] || r.data[session].type === "idle"); });
  async function prompt(text: string) {
    const before = captures.length;
    const r = await client.session.promptAsync({ path: { id: session }, body: { parts: [{ type: "text", text }] } }); assert(!r.error, "Native prompt rejected");
    await waitFor("native provider round", async () => captures.length > before); await idle();
  }
  await brokerRpc(state, { op: "register", agent: { name: "native-probe", provider: "inbox", session: "native-probe", endpoint: "", enabled: true }, replace: false });
  await brokerRpc(state, { op: "send", id: incoming, sender: "native-probe", recipient: selected, body: marker, parent: null });
  await waitFor("actual MCP ack reply and board update", async () => {
    const original = await brokerRpc(state, { op: "get", id: incoming }) as any, reply = await brokerRpc(state, { op: "get", id: replyID }) as any;
    const board = await brokerRpc(state, { op: "board_read" }) as any;
    return discoveryVerified && stage === 6 && original.state === "acknowledged" && reply.parent === incoming && reply.sender === selected && reply.recipient === "native-probe" && board.markdown === ownBoard;
  }, 60000);
  await idle();
  const pairingBefore = JSON.stringify(await brokerRpc(state, { op: "lan_pairing_list" }));
  await command([binary, "--state-dir", state, "pairing", "open", "--agent", selected], true);
  assert(JSON.stringify(await brokerRpc(state, { op: "lan_pairing_list" })) === pairingBefore, "CLI pairing preflight changed window/exchange state");
  phase = "pairing"; stage = 0; await prompt("Check unavailable native pairing owner input without opening any pairing window.");
  assert(pairingDenied, "Native MCP open_pairing preflight was not exercised");
  assert(JSON.stringify(await brokerRpc(state, { op: "lan_pairing_list" })) === pairingBefore, "MCP pairing preflight changed window/exchange state");
  phase = "passive";
  const transcript = await messages(), userPeer = transcript.find(m => m.info.role === "user" && m.parts.some(p => p.type === "text" && p.text.includes(marker)));
  assert(userPeer, "Broker peer message missing from actual native transcript");
  const text = userPeer.parts.filter(p => p.type === "text").map(p => p.text).join("");
  assert(text.includes("native-probe") && text.includes(marker) && text.includes(incoming), "Peer message lost actual sender/body/correlation");
  assert(!text.includes("[Agent Bridge") && !text.includes("<agent-bridge-session-context>") && !text.includes("ack --state-dir"), "Peer payload contains transport branding or repeated setup");
  await Bun.sleep(3400); assert((await notices()).length === 0, "Attributed own board write notified its author");
  const editBoard = async (markdown: string) => {
    const board = await brokerRpc(state, { op: "board_read" }) as any;
    await brokerRpc(state, { op: "board_update", expected_revision: board.revision, edits: [{ op: "set", markdown }], actor: "native-probe" });
  };
  await brokerRpc(state, { op: "pause", paused: true });
  await editBoard("Other writer | needs owner\n"); await editBoard("Other writer | needs review\n");
  await Bun.sleep(3400); assert((await notices()).length === 0, "Pause did not hold board notification");
  phase = "notice"; stage = 0;
  await brokerRpc(state, { op: "pause", paused: false });
  await waitFor("coalesced board notification", async () => (await notices()).length === 1);
  await waitFor("notice MCP board read", async () => stage === 1); await idle();
  phase = "unsubscribe"; stage = 0; await prompt("Stop board notifications for this disposable session.");
  await editBoard("Other writer | after unsubscribe\n"); await Bun.sleep(3400);
  assert((await notices()).length === 1, "Unsubscribed native session received another board notice");
  phase = "compaction"; stage = 0;
  const beforeCompaction = captures.length;
  const summarized = await client.session.summarize({ path: { id: session }, body: { providerID: "fixture", modelID: "fixture" } });
  assert(!summarized.error, "Actual native compaction failed");
  await waitFor("actual compaction context", async () => captures.slice(beforeCompaction).some(c => c.compaction)); await idle();
  assert(captures.slice(beforeCompaction).filter(c => c.compaction).every(c => c.systemBlocks === 1 && c.exactIdentity), "Compaction request lost exact session system context");
  assert((await messages()).some(m => m.info.role === "assistant" && m.info.summary), "Native summary message was not persisted");
  phase = "after-compaction"; await prompt("Continue the disposable session after compaction.");
  assert(captures.some(c => c.phase === phase && c.tools > 0 && c.exactIdentity), "Context missing after compaction");
  await stop(native); native = undefined;
  await waitFor("plugin lifecycle close", async () => !(await brokerRpc(state, { op: "list", all: false }) as any[]).some(a => a.name === selected));
  native = startNative(); await healthy();
  phase = "after-reload"; await prompt("Explicitly resume this same disposable native session after server reload.");
  await registration(); assert(captures.some(c => c.phase === phase && c.tools > 0 && c.exactIdentity), "Context missing after reload/resume");
  phase = "subscribe"; stage = 0; await prompt("Opt this disposable session into board notifications again.");
  assert(stage === 1, "Reloaded native session did not invoke actual subscribe MCP tool");
  const removed = await client.session.delete({ path: { id: session } }); assert(!removed.error, "Disposable session deletion failed");
  await waitFor("deleted session unregister", async () => !(await brokerRpc(state, { op: "list", all: false }) as any[]).some(a => a.name === selected));
  const afterEnd = captures.length;
  await editBoard("Other writer | after session end\n"); await Bun.sleep(3400);
  assert(captures.length === afterEnd, "Session cleanup retained a board notification/model task");
  assert(modelErrors.length === 0, `Local model assertions: ${modelErrors.join("; ")}`);
  assert(captures.filter(c => c.tools > 0).every(c => c.systemBlocks === 1 && c.exactIdentity && !c.transcriptSetup), "Per-request context invariant failed");
  console.log(JSON.stringify({ result: "passed", opencode: "1.18.35", installed_assets: true, installed_mcp: true, exact_session: session, incoming, reply: replyID, parent: incoming, model_requests: captures.length, context_requests: captures.filter(c => c.tools > 0).length, compaction_context: true, context_after_compaction: true, context_after_reload: true, compact_peer_body: true, real_mcp_discovery: true, real_mcp_ack_reply: true, mcp_board_read_update: true, board_opt_in: true, cli_mcp_pairing_preflight: true, own_write_suppression: true, paused_coalesced_notification: true, unsubscribe: true, deleted_registration_cleanup: true }));
  passed = true;
} finally {
  await Promise.all(children.map(stop)); model.stop(true);
  const output = await Promise.all(logs);
  if (!passed) {
    await writeFile(join(root, "native-logs.txt"), output.join("\n"));
    await writeFile(join(root, "model-captures.json"), JSON.stringify({ captures, modelErrors }, null, 2));
    console.error(`Disposable failure evidence retained at ${root}`);
  } else await rm(root, { recursive: true, force: true });
}
