/** Disposable real OpenCode V1 + broker + MCP roundtrip using a scripted local
 * OpenAI-compatible model fixture. This validates native execution/tool wiring;
 * the fixture is not an independent language model or proof of model judgement. */
import { createOpencodeClient } from "@opencode-ai/sdk";
import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import { connect } from "node:net";
import { brokerRpc } from "./rpc.ts";

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
const incoming = randomUUID(), replyID = randomUUID();
const marker = `AGB3-NATIVE-${randomUUID()}`;
let selected = "", stage = 0, modelRequests = 0;
const model = Bun.serve({ hostname: "127.0.0.1", port: 0, async fetch(request) {
  const body = await request.json() as any;
  modelRequests++;
  const tools = (body.tools ?? []) as any[];
  const text = JSON.stringify(body.messages ?? []);
  let toolCall: any;
  if (text.includes(marker) && tools.length && selected) {
    if (stage === 0) {
      const tool = tools.find(t => t.function?.name.endsWith("acknowledge_message"));
      if (!tool) return Response.json({ error: { message: "Missing broker acknowledge MCP tool" } }, { status: 500 });
      toolCall = { id: "call_ack", type: "function", function: { name: tool.function.name, arguments: JSON.stringify({ id: incoming, agent: selected }) } };
      stage = 1;
    } else if (stage === 1) {
      const tool = tools.find(t => t.function?.name.endsWith("send_message"));
      if (!tool) return Response.json({ error: { message: "Missing broker send MCP tool" } }, { status: 500 });
      toolCall = { id: "call_reply", type: "function", function: { name: tool.function.name, arguments: JSON.stringify({ from: selected, to: "native-probe", body: `Reply ${marker}`, id: replyID, parent: incoming }) } };
      stage = 2;
    }
  }
  const content = toolCall ? null : text.includes(marker) ? "Fixture acknowledged and replied through broker MCP tools." : "Disposable probe";
  const response = { id: "chatcmpl-fixture", object: "chat.completion", created: Math.floor(Date.now()/1000), model: "fixture", choices: [{ index: 0, message: { role: "assistant", content, ...(toolCall ? { tool_calls: [toolCall] } : {}) }, finish_reason: toolCall ? "tool_calls" : "stop" }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } };
  if (!body.stream) return Response.json(response);
  const delta = { role: "assistant", ...(toolCall ? { tool_calls: [{ index: 0, ...toolCall }] } : { content }) };
  const chunk = (value: unknown) => `data: ${JSON.stringify(value)}\n\n`;
  return new Response(chunk({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta, finish_reason: null }] }) + chunk({ ...response, object: "chat.completion.chunk", choices: [{ index: 0, delta: {}, finish_reason: toolCall ? "tool_calls" : "stop" }] }) + "data: [DONE]\n\n", { headers: { "content-type": "text/event-stream" } });
}});
const reserve = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response() });
const port = reserve.port!;
reserve.stop(true);
const config = {
  $schema: "https://opencode.ai/config.json",
  enabled_providers: ["fixture"], model: "fixture/fixture", small_model: "fixture/fixture",
  provider: { fixture: { npm: "@ai-sdk/openai-compatible", name: "Fixture", options: { baseURL: `http://127.0.0.1:${model.port}/v1`, apiKey: "local-fixture-only" }, models: { fixture: { name: "Fixture", limit: { context: 8192, output: 2048 }, tool_call: true } } } },
  plugin: [[pathToFileURL(join(dirname(fileURLToPath(import.meta.url)), "index.ts")).href, { stateDir: state }]],
  mcp: { bridge: { type: "local", command: [resolve(binary), "--state-dir", state, "mcp"], enabled: true } },
};
await writeFile(join(workspace, "opencode.json"), JSON.stringify(config));
const broker = Bun.spawn([resolve(binary), "--state-dir", state, "serve", "--no-discovery"], { cwd: workspace, env, stdout: "pipe", stderr: "pipe" });
const native = Bun.spawn([opencode, "serve", "--hostname", "127.0.0.1", "--port", String(port), "--print-logs", "--log-level", "DEBUG"], { cwd: workspace, env, stdout: "pipe", stderr: "pipe" });
const logs = [new Response(broker.stdout).text(), new Response(broker.stderr).text(), new Response(native.stdout).text(), new Response(native.stderr).text()];
let success = false;
async function waitFor<T>(description: string, call: () => Promise<T | undefined | false>, milliseconds = 30000): Promise<T> {
  const deadline = Date.now() + milliseconds;
  let last: unknown;
  do { try { const result = await call(); if (result) return result; } catch (error) { last = error; } await Bun.sleep(100); } while (Date.now() < deadline);
  throw new Error(`Timed out: ${description}${last ? ` (${last})` : ""}`);
}
try {
  await waitFor("broker startup", async () => brokerRpc(state, { op: "status" }));
  const baseUrl = `http://127.0.0.1:${port}`;
  await waitFor("native server startup", async () => (await fetch(baseUrl + "/global/health", { signal: AbortSignal.timeout(2000) })).ok);
  const client = createOpencodeClient({ baseUrl, directory: workspace, fetch: (request) => fetch(request, { signal: AbortSignal.timeout(15000) }) });
  const created = await client.session.create({ body: { title: "Disposable AGB-3 idle probe" } });
  if (created.error || !created.data) throw new Error("Cannot create disposable native session");
  const session = created.data.id;
  selected = `opencode-${session}`;
  const registration = await waitFor<any>("native plugin lifecycle registration", async () => {
    const result = await brokerRpc(state, { op: "list", all: false }) as any;
    const agents = Array.isArray(result) ? result : result.agents;
    return agents?.find((agent: any) => agent.name === selected);
  });
  await brokerRpc(state, { op: "register", agent: { name: "native-probe", provider: "inbox", session: "native-probe", endpoint: "", enabled: true }, replace: false });
  await brokerRpc(state, { op: "send", id: incoming, sender: "native-probe", recipient: selected, body: marker, parent: null });
  await waitFor("explicit native MCP acknowledgement and correlated reply", async () => {
    const original = await brokerRpc(state, { op: "get", id: incoming }) as any;
    const reply = await brokerRpc(state, { op: "get", id: replyID }) as any;
    return original.state === "acknowledged" && reply.parent === incoming && reply.sender === selected && reply.recipient === "native-probe" && reply.body === `Reply ${marker}`;
  }, 60000);
  const messages = await client.session.messages({ path: { id: session } });
  if (!messages.data) throw new Error("Cannot inspect native message receipt");
  const mapped = `msg_${incoming.replaceAll("-", "")}`;
  const receipts = messages.data.filter(message => message.info.id === mapped && message.info.role === "user" && message.info.sessionID === session);
  if (receipts.length !== 1) throw new Error("Expected one exact native user message");
  const content = receipts[0]!.parts.filter(part => part.type === "text").map(part => part.text).join("");
  const retry = await new Promise<any>((accept, reject) => {
    const socket = connect(registration.endpoint);
    let response = "";
    socket.setTimeout(5000, () => { socket.destroy(); reject(new Error("Native retry receipt timeout")); });
    socket.on("error", reject);
    socket.on("connect", () => socket.write(JSON.stringify({ op: "deliver", session, id: incoming, content }) + "\n"));
    socket.on("data", chunk => {
      response += chunk.toString();
      if (!response.includes("\n")) return;
      socket.destroy();
      try { accept(JSON.parse(response.split("\n")[0]!)); } catch (error) { reject(error); }
    });
    socket.on("end", () => { if (!response.includes("\n")) reject(new Error("Missing native retry receipt")); });
  });
  if (retry.ok !== true || retry.id !== incoming || retry.session !== session || retry.native_message_id !== mapped) throw new Error("Ordinary native retry did not return the same exact receipt");
  const afterRetry = await client.session.messages({ path: { id: session } });
  if (afterRetry.data?.filter(message => message.info.id === mapped).length !== 1) throw new Error("Native duplicate after ordinary retry");
  console.log(JSON.stringify({ result: "passed", opencode: "1.18.35", model: "scripted localhost OpenAI-compatible fixture", model_requests: modelRequests, exact_session: session, native_message_id: mapped, broker_id: incoming, reply_id: replyID, parent: incoming, explicit_acknowledgement: true, correlated_reply: true, ordinary_native_retry: true, native_user_messages: receipts.length }));
  success = true;
} finally {
  async function stopChild(child: ReturnType<typeof Bun.spawn>) {
    child.kill("SIGTERM");
    const deadline = setTimeout(() => child.kill("SIGKILL"), 5000);
    try { await child.exited; } finally { clearTimeout(deadline); }
  }
  await Promise.all([stopChild(native), stopChild(broker)]);
  model.stop(true);
  const output = await Promise.all(logs);
  if (!success) console.error(output.filter(Boolean).join("\n"));
  await rm(root, { recursive: true, force: true });
}
