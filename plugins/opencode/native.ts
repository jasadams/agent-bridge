/** Actual pinned OpenCode + broker/MCP execution, driven by a local scripted
 * model. Every session, credential placeholder and configuration is disposable.
 * The busy-race case demonstrates native behavior, not an atomic broker reserve. */
import { createOpencodeClient } from "@opencode-ai/sdk";
import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import { connect } from "node:net";
import { Database } from "bun:sqlite";
import { brokerRpc } from "./rpc.ts";
import { nativeID } from "./journal.ts";

const binary = process.argv[2];
if (!binary) throw new Error("Usage: bun native.ts /absolute/path/to/agent-bridge [opencode]");
const opencode = process.argv[3] ?? "opencode";
const version = Bun.spawnSync([opencode, "--version"]);
if (version.exitCode !== 0 || version.stdout.toString().trim() !== "1.18.35") throw new Error("Native harness requires OpenCode 1.18.35 exactly");
const root = await mkdtemp("/tmp/aboc-native-");
await chmod(root, 0o700);
const home = join(root, "home"), workspace = join(root, "workspace"), state = join(root, "state");
for (const path of [home, workspace, state]) await mkdir(path, { mode: 0o700 });
const env = {
  PATH: process.env.PATH ?? "/usr/bin:/bin", HOME: home,
  XDG_CONFIG_HOME: join(home, ".config"), XDG_DATA_HOME: join(home, ".local/share"), XDG_CACHE_HOME: join(home, ".cache"), XDG_STATE_HOME: join(home, ".local/state"),
  OPENCODE_DISABLE_DEFAULT_PLUGINS: "true", OPENCODE_DISABLE_MODELS_FETCH: "true", OPENCODE_DISABLE_AUTOUPDATE: "true", OPENCODE_DISABLE_SHARE: "true", OPENCODE_DISABLE_MCP_OAUTH: "true", DO_NOT_TRACK: "1",
};
const incoming = randomUUID(), replyID = randomUUID(), marker = `AGB4-IDLE-${randomUUID()}`;
const busyMarker = `AGB4-BUSY-${randomUUID()}`, raceMarker = `AGB4-RACE-${randomUUID()}`, permissionMarker = `AGB4-PERMISSION-${randomUUID()}`;
const started = new Set<string>(), finished = new Set<string>(), aborted = new Set<string>(), permissionPending = new Set<string>(), permissionFinished = new Set<string>();
const gates = new Map<string, () => void>(), stages = new Map<string, number>();
let selected = "", modelRequests = 0, embeddedBun = "";
const model = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
  const url = new URL(request.url), token = url.searchParams.get("token") ?? "";
  if (url.pathname === "/block") {
    started.add(token); embeddedBun = url.searchParams.get("runtime") ?? "";
    await new Promise<void>(release => gates.set(token, release));
    return new Response("released");
  }
  if (url.pathname === "/complete") { finished.add(token); return new Response("ok"); }
  if (url.pathname === "/aborted") { aborted.add(token); return new Response("ok"); }
  if (url.pathname === "/permission-start") { embeddedBun = url.searchParams.get("runtime") ?? ""; return new Response("ok"); }
  if (url.pathname === "/permission-finished") { permissionFinished.add(token); return new Response("ok"); }
  if (url.pathname === "/event") {
    const event = await request.json() as any;
    if (["permission.asked", "permission.updated"].includes(event.type)) {
      const info = event.properties.info ?? event.properties;
      if (typeof info.sessionID === "string") permissionPending.add(info.sessionID);
    }
    return new Response("ok");
  }
  const body = await request.json() as any;
  modelRequests++;
  const tools = (body.tools ?? []) as any[], text = JSON.stringify(body.messages ?? []);
  let toolCall: any;
  const tool = (suffix: string, args: unknown, id: string) => {
    const found = tools.find(t => t.function?.name.endsWith(suffix));
    if (!found) throw new Error(`Missing native fixture/MCP tool: ${suffix}`);
    return { id, type: "function", function: { name: found.function.name, arguments: JSON.stringify(args) } };
  };
  // Title/summarization calls have no tools; never advance scenario state there.
  if (tools.length) {
    const scenario = [permissionMarker, raceMarker, busyMarker, marker].find(value => text.includes(value));
    const stage = scenario ? stages.get(scenario) ?? 0 : -1;
    if (scenario === marker && selected) {
      if (stage === 0) toolCall = tool("acknowledge_message", { id: incoming, agent: selected }, "call_ack");
      if (stage === 1) toolCall = tool("send_message", { from: selected, to: "native-probe", body: `Reply ${marker}`, id: replyID, parent: incoming }, "call_reply");
    } else if (scenario && stage === 0) {
      toolCall = tool(scenario === permissionMarker ? "fixture_permission" : "fixture_block", { token: scenario }, `call_${scenario.slice(0, 16)}`);
    }
    if (scenario && toolCall) stages.set(scenario, stage + 1);
  }
  const content = toolCall ? null : "Disposable native fixture complete.";
  const response = { id: "chatcmpl-fixture", object: "chat.completion", created: Math.floor(Date.now()/1000), model: "fixture", choices: [{ index: 0, message: { role: "assistant", content, ...(toolCall ? { tool_calls: [toolCall] } : {}) }, finish_reason: toolCall ? "tool_calls" : "stop" }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } };
  if (!body.stream) return Response.json(response);
  const delta = { role: "assistant", ...(toolCall ? { tool_calls: [{ index: 0, ...toolCall }] } : { content }) };
  const chunk = (value: unknown) => `data: ${JSON.stringify(value)}\n\n`;
  return new Response(chunk({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta, finish_reason: null }] }) + chunk({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta: {}, finish_reason: toolCall ? "tool_calls" : "stop" }] }) + "data: [DONE]\n\n", { headers: { "content-type": "text/event-stream" } });
}});
const reserve = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response() });
const port = reserve.port!; reserve.stop(true);
const pluginDirectory = dirname(fileURLToPath(import.meta.url));
const fixturePlugin = join(workspace, "fixture-plugin.ts"), origin = `http://127.0.0.1:${model.port}`;
// This separate test plugin provides controlled native work and observes native
// events. Production code and owner rules are untouched; no approval is answered.
await writeFile(fixturePlugin, `import {tool} from ${JSON.stringify(pathToFileURL(join(pluginDirectory, "node_modules/@opencode-ai/plugin/dist/tool.js")).href)};
const origin=${JSON.stringify(origin)};
const ping=(path,token)=>fetch(origin+path+'?token='+encodeURIComponent(token)+'&runtime='+Bun.version);
export default async()=>({
 event:async({event})=>{if(event.type.startsWith('permission.'))await fetch(origin+'/event',{method:'POST',body:JSON.stringify(event)});},
 tool:{
 fixture_block:tool({description:'Disposable native blocking fixture',args:{token:tool.schema.string()},execute:async({token},ctx)=>{let complete=false;ctx.abort.addEventListener('abort',()=>{if(!complete)void ping('/aborted',token)},{once:true});await ping('/block',token);await ping('/complete',token);complete=true;return 'native fixture tool finished';}}),
 fixture_permission:tool({description:'Disposable native permission fixture',args:{token:tool.schema.string()},execute:async({token},ctx)=>{await ping('/permission-start',token);await ctx.ask({permission:'fixture_permission',patterns:[token],always:[],metadata:{fixture:true}});await ping('/permission-finished',token);return 'permission answered';}})
 }});
`);
const config = {
  $schema: "https://opencode.ai/config.json",
  enabled_providers: ["fixture"], model: "fixture/fixture", small_model: "fixture/fixture",
  provider: { fixture: { npm: "@ai-sdk/openai-compatible", name: "Fixture", options: { baseURL: origin + "/v1", apiKey: "local-fixture-only" }, models: { fixture: { name: "Fixture", limit: { context: 8192, output: 2048 }, tool_call: true } } } },
  plugin: [[pathToFileURL(join(pluginDirectory, "index.ts")).href, { stateDir: state }], pathToFileURL(fixturePlugin).href],
  // Only this generated disposable fixture asks. Neither adapter nor plugin
  // modifies permissions or submits a native permission response.
  permission: { fixture_permission: "ask" },
  mcp: { bridge: { type: "local", command: [resolve(binary), "--state-dir", state, "mcp"], enabled: true } },
};
await writeFile(join(workspace, "opencode.json"), JSON.stringify(config));
const broker = Bun.spawn([resolve(binary), "--state-dir", state, "serve", "--no-discovery"], { cwd: workspace, env, stdout: "pipe", stderr: "pipe" });
const native = Bun.spawn([opencode, "serve", "--hostname", "127.0.0.1", "--port", String(port), "--print-logs", "--log-level", "DEBUG"], { cwd: workspace, env, stdout: "pipe", stderr: "pipe" });
const logs = [new Response(broker.stdout).text(), new Response(broker.stderr).text(), new Response(native.stdout).text(), new Response(native.stderr).text()];
let success = false;
function assert(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
async function waitFor<T>(description: string, call: () => Promise<T | undefined | false>, milliseconds = 30000): Promise<T> {
  const deadline = Date.now() + milliseconds; let last: unknown;
  do { try { const result = await call(); if (result) return result; } catch (error) { last = error; } await Bun.sleep(100); } while (Date.now() < deadline);
  throw new Error(`Timed out: ${description}${last ? ` (${last})` : ""}`);
}
function query<T>(sql: string, ...bindings: string[]): T {
  const db = new Database(join(state, "messages.sqlite3"), { readonly: true });
  try { return db.query(sql).get(...bindings) as T; } finally { db.close(); }
}
function socketFrame(endpoint: string, frame: unknown): Promise<any> {
  return new Promise((accept, reject) => {
    const socket = connect(endpoint); let response = "";
    socket.setTimeout(5000, () => { socket.destroy(); reject(new Error("Native socket receipt timeout")); });
    socket.on("error", reject);
    socket.on("connect", () => socket.write(JSON.stringify(frame) + "\n"));
    socket.on("data", chunk => { response += chunk.toString(); if (response.includes("\n")) { socket.destroy(); try { accept(JSON.parse(response.split("\n")[0]!)); } catch (error) { reject(error); } } });
    socket.on("end", () => { if (!response.includes("\n")) reject(new Error("Missing socket receipt")); });
  });
}
try {
  await waitFor("broker startup", async () => brokerRpc(state, { op: "status" }));
  const baseUrl = `http://127.0.0.1:${port}`;
  await waitFor("native startup", async () => (await fetch(baseUrl + "/global/health", { signal: AbortSignal.timeout(2000) })).ok);
  const client = createOpencodeClient({ baseUrl, directory: workspace, fetch: request => fetch(request, { signal: AbortSignal.timeout(60000) }) });
  async function session(title: string) {
    const created = await client.session.create({ body: { title } });
    assert(created.data && !created.error, "Cannot create disposable native session");
    const id = created.data.id;
    const registration = await waitFor<any>("native lifecycle registration", async () => {
      const result = await brokerRpc(state, { op: "list", all: false }) as any;
      const agents = Array.isArray(result) ? result : result.agents;
      return agents?.find((agent: any) => agent.name === `opencode-${id}`);
    });
    return { id, registration };
  }
  async function prompt(id: string, text: string, messageID = `msg_fixture_${randomUUID().replaceAll("-", "")}`) {
    const result = await client.session.promptAsync({ path: { id }, body: { messageID, parts: [{ type: "text", text }] } });
    assert(!result.error, "Native fixture prompt rejected"); return messageID;
  }
  async function idle(id: string) { const status = await client.session.status(); return !!status.data && (!status.data[id] || status.data[id].type === "idle"); }
  async function nativeMessage(id: string, messageID: string) { const result = await client.session.message({ path: { id, messageID } }); return result.data; }
  const first = await session("Disposable AGB-4 idle probe"); selected = `opencode-${first.id}`;
  await brokerRpc(state, { op: "register", agent: { name: "native-probe", provider: "inbox", session: "native-probe", endpoint: "", enabled: true }, replace: false });
  await brokerRpc(state, { op: "send", id: incoming, sender: "native-probe", recipient: selected, body: marker, parent: null });
  await waitFor("native MCP acknowledgement and correlated reply", async () => {
    const original = await brokerRpc(state, { op: "get", id: incoming }) as any, reply = await brokerRpc(state, { op: "get", id: replyID }) as any;
    return original.state === "acknowledged" && reply.parent === incoming && reply.sender === selected && reply.recipient === "native-probe" && reply.body === `Reply ${marker}`;
  }, 60000);
  const dispatch = query<any>("SELECT * FROM opencode_deliveries WHERE id=?", incoming);
  assert(dispatch?.namespace && dispatch.attempt && dispatch.native_id, "Missing durable native dispatch mapping");
  const mapped = nativeID(dispatch.namespace, first.id, incoming);
  assert(mapped === dispatch.native_id, "Broker and plugin native mapping disagree");
  const receipt = await nativeMessage(first.id, mapped);
  assert(receipt?.info.id === mapped && receipt.info.role === "user" && receipt.info.sessionID === first.id, "Missing exact native insertion");
  const before = new Database(join(state, "opencode-receipts.sqlite3"), { readonly: true });
  const journalBefore = before.query("SELECT * FROM receipts WHERE namespace=? AND id=?").get(dispatch.namespace, incoming); before.close();
  const reconciled = await socketFrame(first.registration.endpoint, { op: "reconcile", session: first.id, id: incoming, content: dispatch.content, namespace: dispatch.namespace, generation: dispatch.generation, attempt: dispatch.attempt, native_message_id: mapped });
  assert(reconciled.ok === true && reconciled.status === "inserted" && reconciled.native_message_id === mapped, "Read-only native reconciliation failed");
  const after = new Database(join(state, "opencode-receipts.sqlite3"), { readonly: true });
  const journalAfter = after.query("SELECT * FROM receipts WHERE namespace=? AND id=?").get(dispatch.namespace, incoming); after.close();
  assert(JSON.stringify(journalBefore) === JSON.stringify(journalAfter), "Reconciliation changed the journal");
  const history = await client.session.messages({ path: { id: first.id } });
  assert(history.data?.filter(item => item.info.id === mapped).length === 1, "Native retry duplicated the user message");
  await waitFor("idle after MCP fixture", () => idle(first.id));

  const busy = await session("Disposable busy-tool probe");
  await prompt(busy.id, busyMarker);
  await waitFor("actual native blocking tool start", async () => started.has(busyMarker));
  assert(!await idle(busy.id), "Blocking native tool must report busy");
  const busyID = randomUUID();
  await brokerRpc(state, { op: "send", id: busyID, sender: "native-probe", recipient: busy.registration.name, body: "Broker peer waits for native tool", parent: null });
  const busyDispatch = query<any>("SELECT * FROM opencode_deliveries WHERE id=?", busyID);
  const busyNative = nativeID(busyDispatch.namespace, busy.id, busyID);
  // Bound observation spans a broker scheduling tick. It never drives delivery.
  await Bun.sleep(1200);
  const queuedBusy = await brokerRpc(state, { op: "get", id: busyID }) as any;
  assert(queuedBusy.state === "queued", "Busy-tool peer work must stay broker-queued");
  assert(!await nativeMessage(busy.id, busyNative), "Busy-tool peer prompt became visible before release");
  assert(!finished.has(busyMarker) && !aborted.has(busyMarker), "Delivery completed or canceled a held native tool");
  await brokerRpc(state, { op: "pause", paused: true });
  const cancelledID = randomUUID();
  await brokerRpc(state, { op: "send", id: cancelledID, sender: "native-probe", recipient: busy.registration.name, body: "Cancel this still-queued peer work", parent: null });
  await brokerRpc(state, { op: "cancel", id: cancelledID });
  const cancelledDispatch = query<any>("SELECT * FROM opencode_deliveries WHERE id=?", cancelledID);
  const cancelledNative = nativeID(cancelledDispatch.namespace, busy.id, cancelledID);
  gates.get(busyMarker)!();
  await waitFor("native blocking tool completion", async () => finished.has(busyMarker));
  await waitFor("native idle while broker paused", () => idle(busy.id));
  await Bun.sleep(1200);
  const pausedMessage = await brokerRpc(state, { op: "get", id: busyID }) as any;
  const cancelledMessage = await brokerRpc(state, { op: "get", id: cancelledID }) as any;
  assert(pausedMessage.state === "queued" && !await nativeMessage(busy.id, busyNative), "Broker pause failed across native busy-to-idle transition");
  assert(cancelledMessage.state === "cancelled" && !await nativeMessage(busy.id, cancelledNative), "Broker cancelled peer work reached the native session");
  await brokerRpc(state, { op: "pause", paused: false });
  await waitFor("queued peer inserted after unpause and native tool completion", async () => !!await nativeMessage(busy.id, busyNative));
  assert(!aborted.has(busyMarker), "Native work was canceled during deferred delivery");

  const race = await session("Disposable native idle-to-busy race");
  assert(await idle(race.id), "Race begins with a native idle snapshot");
  await prompt(race.id, raceMarker);
  await waitFor("native work starts after idle snapshot", async () => started.has(raceMarker));
  assert(!await idle(race.id), "Race native tool must be busy before insertion");
  // Native baseline: a stale idle snapshot cannot reserve the runner. The actual
  // V1 prompt API inserts a user message while an existing runner is still busy.
  // Broker code cannot eliminate this last race without a native atomic API.
  const raceNative = await prompt(race.id, "Disposable race insertion after stale idle snapshot");
  await waitFor("native race user message visibility", async () => !!await nativeMessage(race.id, raceNative));
  assert(!finished.has(raceMarker) && !aborted.has(raceMarker), "Race injection changed existing work");
  gates.get(raceMarker)!();
  await waitFor("race native tool completes without cancellation", async () => finished.has(raceMarker));
  assert(!aborted.has(raceMarker), "Native idle/busy race canceled the original tool");

  const permission = await session("Disposable permission-wait probe");
  await prompt(permission.id, permissionMarker);
  await waitFor("actual unanswered native permission request", async () => permissionPending.has(permission.id));
  assert(!await idle(permission.id), "Native permission wait should remain busy");
  const permissionID = randomUUID();
  await brokerRpc(state, { op: "send", id: permissionID, sender: "native-probe", recipient: permission.registration.name, body: "Broker peer must not answer native permission", parent: null });
  const permissionDispatch = query<any>("SELECT * FROM opencode_deliveries WHERE id=?", permissionID);
  const permissionNative = nativeID(permissionDispatch.namespace, permission.id, permissionID);
  await Bun.sleep(1200);
  const queuedPermission = await brokerRpc(state, { op: "get", id: permissionID }) as any;
  assert(queuedPermission.state === "queued" && !await nativeMessage(permission.id, permissionNative), "Permission wait admitted a peer prompt");
  assert(!permissionFinished.has(permissionMarker), "Delivery answered a native permission prompt");
  console.log(JSON.stringify({ result: "passed", opencode: "1.18.35", standalone_bun: Bun.version, embedded_bun: embeddedBun, model: "scripted localhost OpenAI-compatible fixture", model_requests: modelRequests, exact_session: first.id, native_message_id: mapped, broker_id: incoming, reply_id: replyID, parent: incoming, explicit_acknowledgement: true, correlated_reply: true, readonly_reconciliation: true, native_user_messages: 1, busy_tool_deferred_until_completion: true, broker_pause_preserved: true, queued_cancel_preserved: true, permission_wait_queued_unanswered: true, idle_to_busy_native_insertion_visible: true, native_cancel_calls: 0, permission_reply_calls: 0 }));
  success = true;
} finally {
  async function stopChild(child: ReturnType<typeof Bun.spawn>) {
    child.kill("SIGTERM"); const deadline = setTimeout(() => child.kill("SIGKILL"), 5000);
    try { await child.exited; } finally { clearTimeout(deadline); }
  }
  await Promise.all([stopChild(native), stopChild(broker)]);
  for (const release of gates.values()) release();
  model.stop(true);
  const output = await Promise.all(logs); if (!success) console.error(output.filter(Boolean).join("\n"));
  await rm(root, { recursive: true, force: true });
}
