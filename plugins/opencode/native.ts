/** Actual pinned OpenCode + broker/MCP execution, driven by a local scripted
 * model. Every session, credential placeholder and configuration is disposable.
 * Native inbox acceptance and later model visibility are verified separately. */
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createHash, randomUUID } from "node:crypto";
import { connect } from "node:net";
import { Database } from "bun:sqlite";
import { brokerRpc } from "./rpc.ts";
import { nativeID } from "./journal.ts";

const binary = process.argv[2];
if (!binary) throw new Error("Usage: bun native.ts /absolute/path/to/agent-bridge [opencode]");
const opencode = process.argv[3] ?? "opencode";
const version = Bun.spawnSync([opencode, "--version"]);
if (version.exitCode !== 0 || version.stdout.toString().trim() !== "opencode v2.2.5") throw new Error("Native harness requires OpenCode V2.2.5 exactly");
const root = await mkdtemp("/tmp/opencode/aboc-native-");
await chmod(root, 0o700);
const home = join(root, "home"), workspace = join(root, "workspace"), state = join(root, "state");
for (const path of [home, workspace, state]) await mkdir(path, { mode: 0o700 });
const env = {
  PATH: process.env.PATH ?? "/usr/bin:/bin", HOME: home,
  XDG_CONFIG_HOME: join(home, ".config"), XDG_DATA_HOME: join(home, ".local/share"), XDG_CACHE_HOME: join(home, ".cache"), XDG_STATE_HOME: join(home, ".local/state"),
  OPENCODE_DISABLE_DEFAULT_PLUGINS: "true", OPENCODE_DISABLE_MODELS_FETCH: "true", OPENCODE_DISABLE_AUTOUPDATE: "true", OPENCODE_DISABLE_SHARE: "true", OPENCODE_DISABLE_MCP_OAUTH: "true", DO_NOT_TRACK: "1",
  OPENCODE_PASSWORD: randomUUID(), OPENCODE_LOG_LEVEL: "debug",
};
const incoming = randomUUID(), replyID = randomUUID(), marker = `AGB4-IDLE-${randomUUID()}`;
const normalID = randomUUID(), normalMarker = `SEND-CONTINUE-${randomUUID()}`;
let normalInputSeen = false;
const busyMarker = `AGB4-BUSY-${randomUUID()}`, raceMarker = `AGB4-RACE-${randomUUID()}`, permissionMarker = `AGB4-PERMISSION-${randomUUID()}`;
const longMarker = `AGB16-LONG-${randomUUID()}`, streamMarker = `AGB16-STREAM-${randomUUID()}`, errorMarker = `AGB16-ERROR-${randomUUID()}`;
const captures = new Map<string, any[]>(), nativeEvents: any[] = [];
const nativeReceipts: unknown[] = [];
const streamStarted = new Set<string>(), streamFinished = new Set<string>(), streamAborted = new Set<string>();
const started = new Set<string>(), finished = new Set<string>(), aborted = new Set<string>(), permissionPending = new Set<string>(), permissionFinished = new Set<string>();
const gates = new Map<string, () => void>(), stages = new Map<string, number>();
let selected = "", modelRequests = 0, embeddedBun = "";
// The fixture deliberately holds /block beyond Bun's default 10-second idle
// timeout. Scenario waits and assertions below still bound the native test.
const model = Bun.serve({ hostname: "127.0.0.1", port: 0, idleTimeout: 0, async fetch(request) {
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
    nativeEvents.push({ time: Date.now(), ...event });
    if (["permission.asked", "permission.updated"].includes(event.type)) {
      const info = event.data;
      if (typeof info.sessionID === "string") permissionPending.add(info.sessionID);
    }
    return new Response("ok");
  }
  const body = await request.json() as any;
  modelRequests++;
  const tools = (body.tools ?? []) as any[], text = JSON.stringify(body.messages ?? []);
  if (tools.length && text.includes(normalMarker)) normalInputSeen = true;
  let toolCall: any;
  const scenario = tools.length ? [permissionMarker, longMarker, streamMarker, errorMarker, raceMarker, busyMarker, marker].find(value => text.includes(value)) : undefined;
  const stage = scenario ? stages.get(scenario) ?? 0 : -1;
  if (scenario) {
    const values = captures.get(scenario) ?? [];
    values.push({ time: Date.now(), model: body.model, messages: body.messages, tools: body.tools }); captures.set(scenario, values);
    stages.set(scenario, stage + 1);
  }
  const tool = (suffix: string, args: unknown, id: string) => {
    const found = tools.find(t => t.function?.name.endsWith(suffix));
    if (!found) throw new Error(`Missing native fixture/MCP tool: ${suffix}; exposed: ${JSON.stringify(tools.map(t => t.function?.name ?? t.name))}`);
    return { id, type: "function", function: { name: found.function.name, arguments: JSON.stringify(args) } };
  };
  // Title/summarization calls have no tools; never advance scenario state there.
  if (tools.length) {
    if (scenario === marker && selected) {
      if (stage === 0) toolCall = tool("send_message", { from: selected, to: "native-probe", body: `Reply ${marker}`, id: replyID, parent: incoming }, "call_reply");
    } else if (scenario && stage === 0 && ![streamMarker, errorMarker].includes(scenario)) {
      toolCall = tool(scenario === permissionMarker ? "fixture_permission" : "fixture_block", { token: scenario }, `call_${scenario.slice(0, 16)}`);
    }
  }
  const content = toolCall ? null : "Disposable native fixture complete.";
  const response = { id: "chatcmpl-fixture", object: "chat.completion", created: Math.floor(Date.now()/1000), model: "fixture", choices: [{ index: 0, message: { role: "assistant", content, ...(toolCall ? { tool_calls: [toolCall] } : {}) }, finish_reason: toolCall ? "tool_calls" : "stop" }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } };
  if (!body.stream) return Response.json(response);
  const delta = { role: "assistant", ...(toolCall ? { tool_calls: [{ index: 0, ...toolCall }] } : { content }) };
  const chunk = (value: unknown) => `data: ${JSON.stringify(value)}\n\n`;
  if (scenario && [streamMarker, errorMarker].includes(scenario) && stage === 0) {
    const heldScenario = scenario;
    request.signal.addEventListener("abort", () => { if (!streamFinished.has(heldScenario)) streamAborted.add(heldScenario); }, { once: true });
    const stream = new ReadableStream({ async start(controller) {
      controller.enqueue(new TextEncoder().encode(chunk({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta: { role: "assistant", content: "Original held assistant context." }, finish_reason: null }] })));
      streamStarted.add(heldScenario);
      await new Promise<void>(release => gates.set(heldScenario, release));
      streamFinished.add(heldScenario);
      const ending = heldScenario === errorMarker ? chunk({ error: { message: "Controlled fixture stream failure", type: "invalid_request_error", code: "fixture_error" } }) : chunk({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta: {}, finish_reason: "stop" }] }) + "data: [DONE]\n\n";
      controller.enqueue(new TextEncoder().encode(ending)); controller.close();
    }});
    return new Response(stream, { headers: { "content-type": "text/event-stream" } });
  }
  return new Response(chunk({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta, finish_reason: null }] }) + chunk({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta: {}, finish_reason: toolCall ? "tool_calls" : "stop" }] }) + "data: [DONE]\n\n", { headers: { "content-type": "text/event-stream" } });
}});
const reserve = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response() });
const port = reserve.port!; reserve.stop(true);
const pluginDirectory = dirname(fileURLToPath(import.meta.url));
const provenance: Record<string, unknown> = {
  opencode: version.stdout.toString().trim(), standalone_bun: Bun.version,
  binary: resolve(binary), binary_sha256: createHash("sha256").update(await readFile(resolve(binary))).digest("hex"),
  source_head: Bun.spawnSync(["git", "rev-parse", "HEAD"], { cwd: pluginDirectory }).stdout.toString().trim(),
  source_status: Bun.spawnSync(["git", "status", "--short"], { cwd: pluginDirectory }).stdout.toString(),
  started_at: new Date().toISOString(),
};
for (const name of ["native.ts", "index.ts", "rpc.ts", "peer.ts", "journal.ts"]) provenance[name + "_sha256"] = createHash("sha256").update(await readFile(join(pluginDirectory, name))).digest("hex");
const fixturePlugin = join(workspace, "fixture-plugin"), origin = `http://127.0.0.1:${model.port}`;
await mkdir(fixturePlugin);
// This separate test plugin provides controlled native work and observes native
// events. Production code and owner rules are untouched; no approval is answered.
await writeFile(join(fixturePlugin, "index.ts"), `import {Plugin} from "@opencode/plugin";
const origin=${JSON.stringify(origin)};
const ping=(path,token)=>fetch(origin+path+'?token='+encodeURIComponent(token)+'&runtime='+Bun.version);
export default Plugin.define({id:'native-fixture',async setup(ctx){
 const controller=new AbortController();
 void(async()=>{for await(const event of ctx.event.subscribe({signal:controller.signal})){if(event.type.startsWith('permission.')||event.type.startsWith('session.execution.')||event.type.startsWith('session.inbox.'))await fetch(origin+'/event',{method:'POST',body:JSON.stringify(event)});}})().catch(error=>{if(!controller.signal.aborted)console.error(error)});
 await ctx.tool.transform(editor=>{
 const input={type:'object',properties:{token:{type:'string'}},required:['token'],additionalProperties:false};
 editor.add({name:'fixture_block',description:'Disposable native blocking fixture',input,options:{codemode:false},execute:async({token},context)=>{let complete=false;context.signal.addEventListener('abort',()=>{if(!complete)void ping('/aborted',token)},{once:true});await ping('/block',token);await ping('/complete',token);complete=true;return {content:'native fixture tool finished'};}});
 editor.add({name:'fixture_permission',description:'Disposable native permission fixture',input,options:{codemode:false},execute:async({token},context)=>{await ping('/permission-start',token);await ctx.permission.assert({sessionID:context.sessionID,agent:context.agent,source:{type:'tool',messageID:context.messageID,id:context.id},action:'fixture_permission',resources:[token],save:[]});await ping('/permission-finished',token);return {content:'permission answered'};}});
 });return()=>controller.abort();}});
`);
const config = {
  $schema: "https://opencode.ai/config.json",
  model: "fixture/fixture", snapshots: false,
  policies: [{ action: "provider.use", resource: "*", effect: "deny" }, { action: "provider.use", resource: "fixture", effect: "allow" }],
  providers: { fixture: { package: "@opencode/ai/providers/openai-compatible", name: "Fixture", settings: { baseURL: origin + "/v1", apiKey: "local-fixture-only" }, models: { fixture: { name: "Fixture", limit: { context: 32768, output: 2048 }, capabilities: { tools: true } }, owner: { name: "Owner fixture", limit: { context: 32768, output: 2048 }, capabilities: { tools: true }, variants: [{ id: "deep" }] } } } },
  agents: { title: { model: "fixture/fixture" }, owner: { description: "Disposable nondefault owner context", mode: "primary", model: "fixture/owner#deep", system: "Retain disposable owner specialist context." } },
  plugins: [{ package: pathToFileURL(pluginDirectory).href, options: { stateDir: state } }, pathToFileURL(fixturePlugin).href],
  // Only this generated disposable fixture asks. Neither adapter nor plugin
  // modifies permissions or submits a native permission response.
  permissions: [{ action: "*", resource: "*", effect: "allow" }, { action: "fixture_permission", resource: "*", effect: "ask" }],
  mcp: { servers: { bridge: { type: "local", command: [resolve(binary), "--state-dir", state, "mcp"], codemode: false } } },
};
await writeFile(join(workspace, "opencode.json"), JSON.stringify(config));
const broker = Bun.spawn([resolve(binary), "--state-dir", state, "serve", "--no-discovery"], { cwd: workspace, env, stdout: "pipe", stderr: "pipe" });
const native = Bun.spawn([opencode, "serve", "--hostname", "127.0.0.1", "--port", String(port)], { cwd: workspace, env, stdout: "pipe", stderr: "pipe" });
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
  const brokerStatus = await waitFor<any>("broker startup", async () => brokerRpc(state, { op: "status" }));
  provenance.broker_build_commit = brokerStatus.build_commit;
  await writeFile(join(root, "provenance.json"), JSON.stringify(provenance, null, 2));
  const baseUrl = `http://127.0.0.1:${port}`;
  const headers = { authorization: "Basic " + Buffer.from(`opencode:${env.OPENCODE_PASSWORD}`).toString("base64"), "content-type": "application/json" };
  async function api(path: string, body?: unknown, method = body === undefined ? "GET" : "POST", unwrap = true): Promise<any> {
    const response = await fetch(baseUrl + path, { headers, method, ...(body === undefined ? {} : { body: JSON.stringify(body) }), signal: AbortSignal.timeout(60000) });
    if (!response.ok) throw new Error(`${method} ${path}: ${response.status} ${await response.text()}`);
    if (response.status === 204) return;
    const value = await response.json() as any; return unwrap ? value.data ?? value : value;
  }
  await waitFor("native startup", () => api("/api/info"));
  const specification = await api("/openapi.json"); await writeFile(join(root, "openapi.json"), JSON.stringify(specification));
  await api("/api/config");
  console.log(JSON.stringify({ root, provenance }));
  await waitFor("connected native MCP", async () => { const servers = await api("/api/mcp"); await writeFile(join(root, "mcp.json"), JSON.stringify(servers)); return JSON.stringify(servers).includes('"connected"'); });
  async function session(title: string) {
    const created = await api("/api/session", { title, location: { directory: workspace } });
    const id = created.id;
    const registration = await waitFor<any>("native lifecycle registration", async () => {
      const result = await brokerRpc(state, { op: "list", all: false }) as any;
      const agents = Array.isArray(result) ? result : result.agents;
      return agents?.find((agent: any) => agent.name === `opencode-${id}`);
    });
    return { id, registration };
  }
  async function prompt(id: string, text: string, messageID = `msg_fixture_${randomUUID().replaceAll("-", "")}`, selection?: { agent: string; model: { providerID: string; id: string; variant: string } }) {
    if (selection) { await api(`/api/session/${id}/agent`, { agent: selection.agent }); await api(`/api/session/${id}/model`, { model: selection.model }); }
    const accepted = await api(`/api/session/${id}/prompt`, { id: messageID, text });
    assert(accepted.id === messageID && accepted.sessionID === id && accepted.type === "user" && accepted.payload.text === text && accepted.delivery === "steer", "V2 native prompt receipt does not match exact input");
    nativeReceipts.push(accepted); return messageID;
  }
  async function idle(id: string) { const active = await api("/api/session/active"); return !active[id]; }
  async function nativeMessage(id: string, messageID: string) { try { return await api(`/api/session/${id}/message/${messageID}`); } catch (error) { if (String(error).includes(": 404 ")) return; throw error; } }
  async function nativeInput(id: string, messageID: string) {
    const projected = await nativeMessage(id, messageID);
    if (projected) return projected;
    const pending = (await api(`/api/session/${id}/inbox`)).filter((item: any) => item.id === messageID);
    assert(pending.length <= 1, "Native inbox duplicated the stable message ID");
    if (!pending.length) return nativeMessage(id, messageID); // Atomic native promotion may cross the reads.
    const item = pending[0];
    assert(item.sessionID === id && item.type === "user" && typeof item.payload.text === "string", "Native inbox identity mismatch");
    return { id: item.id, type: item.type, text: item.payload.text, acceptance: "inbox" };
  }
  async function messages(id: string): Promise<any[]> {
    const rows: any[] = []; let cursor: string | undefined;
    do { const page = await api(`/api/session/${id}/message?limit=200&${cursor ? `cursor=${encodeURIComponent(cursor)}` : "order=asc"}`, undefined, "GET", false); rows.push(...page.data); cursor = page.cursor.next; } while (cursor);
    return rows;
  }
  const first = await session("Disposable AGB-4 idle probe"); selected = `opencode-${first.id}`;
  const loaded = await api("/api/plugin"); await writeFile(join(root, "plugins.json"), JSON.stringify(loaded));
  assert(JSON.stringify(loaded).includes("agent-bridge") && JSON.stringify(loaded).includes("native-fixture"), "Expected V2 plugins were not loaded");
  await brokerRpc(state, { op: "register", agent: { name: "native-probe", provider: "inbox", session: "native-probe", endpoint: "", enabled: true }, replace: false });
  // Ordinary messaging completes without any recipient tool call or reply.
  await brokerRpc(state, { op: "send", id: normalID, sender: "native-probe", recipient: selected, body: normalMarker, parent: null });
  await waitFor("ordinary native input without receipt exchange", async () => {
    const outcome = await brokerRpc(state, { op: "get", id: normalID }) as any;
    return outcome.state === "delivered" && normalInputSeen;
  });
  await waitFor("ordinary native idle", () => idle(first.id));
  const normalDispatch = query<any>("SELECT * FROM opencode_deliveries WHERE id=?", normalID);
  const normalRecord = await nativeMessage(first.id, normalDispatch.native_id);
  assert(normalRecord?.type === "user" && normalRecord.id === normalDispatch.native_id && normalRecord.text.includes(normalMarker), "Ordinary send lacks native input proof");
  const normalHistory = await messages(first.id);
  assert(normalHistory.filter(item => item.id === normalDispatch.native_id).length === 1 && normalHistory.every(item => !item.content?.some((part: any) => part.type === "tool")), "Ordinary send duplicated input or invoked recipient tools");
  assert(query<any>("SELECT count(*) AS count FROM messages WHERE parent=?", normalID).count === 0, "Ordinary send generated a receipt-only reply");
  console.log(JSON.stringify({ phase: "normal_send_continue", native_input: true, recipient_tool_calls: 0, receipt_replies: 0 }));
  await brokerRpc(state, { op: "send", id: incoming, sender: "native-probe", recipient: selected, body: marker, parent: null });
  await waitFor("native acceptance and correlated MCP reply", async () => {
    const original = await brokerRpc(state, { op: "get", id: incoming }) as any, reply = await brokerRpc(state, { op: "get", id: replyID }) as any;
    return original.state === "delivered" && reply.parent === incoming && reply.sender === selected && reply.recipient === "native-probe" && reply.body === `Reply ${marker}`;
  }, 60000);
  const dispatch = query<any>("SELECT * FROM opencode_deliveries WHERE id=?", incoming);
  assert(dispatch?.namespace && dispatch.attempt && dispatch.native_id, "Missing durable native dispatch mapping");
  const mapped = nativeID(dispatch.namespace, first.id, incoming);
  assert(mapped === dispatch.native_id, "Broker and plugin native mapping disagree");
  const receipt = await nativeMessage(first.id, mapped);
  assert(receipt?.id === mapped && receipt.type === "user" && receipt.text === dispatch.content, "Missing exact native insertion");
  const before = new Database(join(state, "opencode-receipts.sqlite3"), { readonly: true });
  const journalBefore = before.query("SELECT * FROM receipts WHERE namespace=? AND id=?").get(dispatch.namespace, incoming); before.close();
  const reconciled = await socketFrame(first.registration.endpoint, { op: "reconcile", session: first.id, id: incoming, content: dispatch.content, namespace: dispatch.namespace, generation: dispatch.generation, attempt: dispatch.attempt, native_message_id: mapped });
  assert(reconciled.ok === true && reconciled.status === "inserted" && reconciled.native_message_id === mapped, "Read-only native reconciliation failed");
  const after = new Database(join(state, "opencode-receipts.sqlite3"), { readonly: true });
  const journalAfter = after.query("SELECT * FROM receipts WHERE namespace=? AND id=?").get(dispatch.namespace, incoming); after.close();
  assert(JSON.stringify(journalBefore) === JSON.stringify(journalAfter), "Reconciliation changed the journal");
  const history = await messages(first.id);
  assert(history.filter(item => item.id === mapped).length === 1, "Native retry duplicated the user message");
  await waitFor("idle after MCP fixture", () => idle(first.id));

  const owner = (variant: string) => ({ agent: "owner", model: { providerID: "fixture", id: "owner", variant } });
  const countText = (capture: any, text: string) => JSON.stringify(capture.messages).split(text).length - 1;
  const waiting = (id: string) => query<any>("SELECT m.state,m.created,d.phase,d.attempt,d.content,d.generation,l.generation AS live_generation,l.ready,l.expires FROM messages m JOIN opencode_deliveries d ON d.id=m.id JOIN opencode_leases l ON l.session=d.session WHERE m.id=?", id);
  async function sendPeer(target: { id: string; registration: any }, body: string, recipient = target.registration.name) {
    const id = randomUUID();
    await brokerRpc(state, { op: "send", id, sender: "native-probe", recipient, body, parent: null });
    const d = query<any>("SELECT * FROM opencode_deliveries WHERE id=?", id);
    return { id, native: nativeID(d.namespace, target.id, id), body };
  }
  async function acceptedInput(target: { id: string }, peer: { id: string; native: string }, variant?: string) {
    const record = await waitFor<any>("verified native client acceptance", async () => {
      const record = await nativeInput(target.id, peer.native);
      const outcome = await brokerRpc(state, { op: "get", id: peer.id }) as any;
      return record && outcome.state === "delivered" && record;
    });
    if (variant) { const selection = await api(`/api/session/${target.id}`); assert(selection.agent === "owner" && selection.model.providerID === "fixture" && selection.model.id === "owner" && selection.model.variant === variant, "Peer insertion changed native owner selection"); }
    const dispatch = query<any>("SELECT content FROM opencode_deliveries WHERE id=?", peer.id);
    assert(record.id === peer.native && record.type === "user" && record.text === dispatch.content, "Accepted native input differs from exact broker content");
    return record;
  }
  async function nextRequest(token: string, peers: Array<{ body: string }>, original: string, sessionID: string, since: number) {
    const request = await waitFor<any>("next normal model request with peer input", async () => captures.get(token)?.[1]);
    assert(request.model === "owner", "Next model request changed selected model");
    assert(peers.every(peer => countText(request, peer.body) === 1), "Next model request did not contain exact peer input once");
    assert(JSON.stringify(request.messages).includes(original), "Next request lost original tool/assistant context");
    const projected = await messages(sessionID);
    for (const peer of peers) assert(projected.filter(item => item.type === "user" && item.text.includes(peer.body)).length === 1, "Next model boundary did not project each native input exactly once");
    assert(!nativeEvents.some(event => event.time >= since && event.time <= request.time && ["session.execution.succeeded", "session.execution.interrupted", "session.execution.failed"].includes(event.type) && event.data.sessionID === sessionID), "Original work ended before consuming active peer input");
    return request;
  }

  // Exercise the actual supported V2 session.move API, not just a mocked
  // location response. Both directories and all native state are disposable.
  const movedDirectory = join(root, "moved-worktree");
  await mkdir(movedDirectory, { recursive: true });
  await api(`/api/session/${first.id}/move`, { directory: movedDirectory });
  await waitFor("native session moved", async () => (await api(`/api/session/${first.id}`)).location.directory === movedDirectory);
  const movedPeer = await sendPeer(first, `Moved session input ${randomUUID()}`);
  await acceptedInput(first, movedPeer);
  const movedInputs = await messages(first.id);
  assert(movedInputs.filter(item => item.id === movedPeer.native).length <= 1, "Moved session duplicated native input");
  console.log(JSON.stringify({ phase: "native_session_move", session: first.id, directory: movedDirectory, delivered: movedPeer.id, native: movedPeer.native }));

  // Seed an upgrade-shaped legacy Unknown only in this disposable broker DB.
  // The schema-12 migration itself is exercised by the Rust migration test;
  // this verifies the resulting row alongside the real V2 native runtime.
  await brokerRpc(state, { op: "pause", paused: true });
  const legacy = await sendPeer(first, `Legacy unknown fixture ${randomUUID()}`);
  const legacyNative = "msg_" + legacy.id.replaceAll("-", "").toLowerCase();
  const fixtureDb = new Database(join(state, "messages.sqlite3"));
  try {
    fixtureDb.transaction(() => {
      fixtureDb.query("UPDATE opencode_deliveries SET phase='legacy',native_id=? WHERE id=?").run(legacyNative, legacy.id);
      fixtureDb.query("UPDATE messages SET state='uncertain',detail='legacy fixture without acceptance proof' WHERE id=?").run(legacy.id);
    }).immediate();
  } finally { fixtureDb.close(); }
  const afterLegacy = await sendPeer(first, `Newer native input after legacy Unknown ${randomUUID()}`);
  await brokerRpc(state, { op: "pause", paused: false });
  await acceptedInput(first, afterLegacy);
  assert((await brokerRpc(state, { op: "get", id: legacy.id }) as any).state === "unknown", "Legacy Unknown was relabeled");
  assert(!await nativeInput(first.id, legacyNative), "Legacy Unknown was replayed");
  const legacyJournal = new Database(join(state, "opencode-receipts.sqlite3"), { readonly: true });
  try { assert(!legacyJournal.query("SELECT id FROM receipts WHERE id=?").get(legacy.id), "Legacy Unknown gained a native attempt"); }
  finally { legacyJournal.close(); }
  console.log(JSON.stringify({ phase: "native_legacy_unknown", legacy: legacy.id, newer: afterLegacy.id, newer_native: afterLegacy.native, legacy_replayed: false }));

  if (process.env.AGENT_BRIDGE_NATIVE_MOVE_ONLY === "1") {
    console.log(JSON.stringify({ result: "passed", scenario: "native_session_move" }));
  } else {
  await brokerRpc(state, { op: "pause", paused: true });
  const long = await session("Disposable AGB-16 active tool and long paused queue");
  await prompt(long.id, longMarker, undefined, owner("default"));
  await waitFor("actual held native tool", async () => started.has(longMarker));
  const toolHeldSince = Date.now();
  const longPeer = await sendPeer(long, `Active tool peer ${randomUUID()}`);
  const alias = `active-alias-${randomUUID()}`;
  await brokerRpc(state, { op: "register", agent: { name: alias, provider: "opencode", session: long.id, endpoint: long.registration.endpoint, enabled: true }, replace: false });
  const aliasPeer = await sendPeer(long, `Second alias peer ${randomUUID()}`, alias);
  const cancelled = await sendPeer(long, "Cancel this paused unattempted input");
  await brokerRpc(state, { op: "cancel", id: cancelled.id });
  const permission = await session("Disposable native owner permission wait");
  await prompt(permission.id, permissionMarker, undefined, owner("deep"));
  await waitFor("unanswered native permission", async () => permissionPending.has(permission.id));
  const permissionPeer = await sendPeer(permission, `Permission queued input ${randomUUID()}`);
  console.log(JSON.stringify({ phase: "holding active native tool and permission with broker paused", milliseconds: 125000 }));
  await Bun.sleep(125000);
  const heldLong = waiting(longPeer.id), heldPermission = waiting(permissionPeer.id);
  for (const held of [heldLong, heldPermission]) {
    assert(Date.now() - held.created > 120000 && held.state === "queued" && held.phase === "queued" && held.attempt === null, "Live paused unattempted work did not survive old expiry");
    assert(held.generation === held.live_generation && held.expires > Date.now(), "Exact native lease was lost during long wait");
  }
  assert(!await nativeMessage(long.id, longPeer.native) && !await nativeMessage(permission.id, permissionPeer.native), "Pause did not prevent native input");
  assert(!finished.has(longMarker) && !aborted.has(longMarker) && !permissionFinished.has(permissionMarker), "Held native work or permission changed");
  await brokerRpc(state, { op: "pause", paused: false });
  const heldToolInput = await acceptedInput(long, longPeer, "default"); await acceptedInput(long, aliasPeer, "default");
  const heldPermissionInput = await acceptedInput(permission, permissionPeer, "deep");
  assert(heldToolInput.acceptance === "inbox" && heldPermissionInput.acceptance === "inbox", "Held work must establish native inbox acceptance before model processing");
  assert(!await idle(long.id) && !finished.has(longMarker) && !aborted.has(longMarker), "Peer input was not inserted during original tool work");
  assert(!await idle(permission.id) && !permissionFinished.has(permissionMarker) && captures.get(permissionMarker)?.length === 1, "Peer input answered permission or started model processing");
  assert(waiting(cancelled.id).state === "cancelled" && !await nativeMessage(long.id, cancelled.native), "Cancelled queued input reached native session");
  const heldInbox = await api(`/api/session/${long.id}/inbox`);
  await writeFile(join(root, "held-inbox-proof.json"), JSON.stringify({ tool: heldInbox, permission: await api(`/api/session/${permission.id}/inbox`), tool_input: heldToolInput, permission_input: heldPermissionInput }, null, 2));
  const userIDs = heldInbox.filter((item: any) => item.type === "user").map((item: any) => item.id);
  assert(userIDs.filter(id => id === longPeer.native).length === 1 && userIDs.indexOf(longPeer.native) < userIDs.indexOf(aliasPeer.native), "Native active input duplicated or bypassed alias FIFO");
  gates.get(longMarker)!();
  await waitFor("original tool completion", async () => finished.has(longMarker));
  const toolRequest = await nextRequest(longMarker, [longPeer, aliasPeer], "native fixture tool finished", long.id, toolHeldSince);
  assert(!aborted.has(longMarker), "Active insertion aborted original native tool");

  const stream = await session("Disposable active native model stream");
  await prompt(stream.id, streamMarker, undefined, owner("deep"));
  await waitFor("actual SSE response held after first chunk", async () => streamStarted.has(streamMarker));
  const streamHeldSince = Date.now(), streamPeer = await sendPeer(stream, `Active stream peer ${randomUUID()}`);
  await acceptedInput(stream, streamPeer, "deep");
  assert(!await idle(stream.id) && !streamFinished.has(streamMarker) && !streamAborted.has(streamMarker), "Peer input did not arrive during held model stream");
  gates.get(streamMarker)!();
  const streamRequest = await nextRequest(streamMarker, [streamPeer], "Original held assistant context.", stream.id, streamHeldSince);
  assert(streamFinished.has(streamMarker) && !streamAborted.has(streamMarker), "Active insertion aborted original model stream");

  const errored = await session("Disposable stream error after native insertion");
  await prompt(errored.id, errorMarker, undefined, owner("default"));
  await waitFor("error fixture stream held", async () => streamStarted.has(errorMarker));
  const errorPeer = await sendPeer(errored, `Peer before controlled stream error ${randomUUID()}`);
  await acceptedInput(errored, errorPeer, "default");
  gates.get(errorMarker)!();
  await waitFor("controlled native stream error", async () => nativeEvents.some(event => event.type === "session.execution.failed" && event.data.sessionID === errored.id));
  await Bun.sleep(1200);
  const errorHistory = await messages(errored.id);
  const errorOutcome = await brokerRpc(state, { op: "get", id: errorPeer.id }) as any;
  const errorInbox = await api(`/api/session/${errored.id}/inbox`);
  assert(errorHistory.filter(item => item.id === errorPeer.native).length + errorInbox.filter((item: any) => item.id === errorPeer.native).length === 1 && errorOutcome.state === "delivered", "Later native error replayed input or erased confirmed acceptance");
  assert(!nativeEvents.some(event => event.type === "permission.replied") && !permissionFinished.has(permissionMarker), "Harness answered owner permission");
  await writeFile(join(root, "checks.json"), JSON.stringify({ result: "passed", ordinary_exact_input: normalInputSeen, ordinary_recipient_tools: 0, ordinary_receipt_replies: 0, correlated_reply: replyID, native_id: mapped, active_tool: longPeer, alias_fifo: aliasPeer, stream: streamPeer, stream_error: errorPeer, unanswered_permission: permissionPeer, long_wait_ms: Date.now() - heldLong.created, model_requests: modelRequests }, null, 2));
  console.log(JSON.stringify({ result: "passed", opencode: "2.2.5", standalone_bun: Bun.version, embedded_bun: embeddedBun, model: "scripted localhost OpenAI-compatible fixture", model_requests: modelRequests, exact_session: first.id, native_acceptance: true, correlated_reply: true, readonly_reconciliation: true, active_tool: { session: long.id, native: longPeer.native, alias_native: aliasPeer.native, original_not_aborted: true, next_request_time: toolRequest.time }, active_stream: { session: stream.id, native: streamPeer.native, original_not_aborted: true, next_request_time: streamRequest.time }, long_wait: { age_ms: Date.now() - heldLong.created, held: heldLong, permission: heldPermission }, permission_input_accepted_unanswered: true, broker_pause_preserved: true, queued_cancel_preserved: true, alias_fifo: true, stream_error_no_replay: true, native_cancel_calls: 0, permission_reply_calls: 0, retry_calls: 0, manual_fetch_calls: 0 }));
  }
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
  await writeFile(join(root, "native-prompt-receipts.json"), JSON.stringify(nativeReceipts, null, 2));
  const logDirectory = process.env.AGENT_BRIDGE_NATIVE_LOG_DIR;
  if (logDirectory) {
    await mkdir(logDirectory, { recursive: true, mode: 0o700 });
    await writeFile(join(logDirectory, "native-events.json"), JSON.stringify(nativeEvents), { mode: 0o600 });
    await writeFile(join(logDirectory, "model-requests.json"), JSON.stringify(Object.fromEntries(captures)), { mode: 0o600 });
    await Promise.all(output.map((value, index) => writeFile(join(logDirectory, ["broker.stdout.log", "broker.stderr.log", "opencode.stdout.log", "opencode.stderr.log"][index]!), value, { mode: 0o600 })));
  }
  if (success && !logDirectory) await rm(root, { recursive: true, force: true });
  else console.error(`Disposable native evidence retained at ${root}`);
}
