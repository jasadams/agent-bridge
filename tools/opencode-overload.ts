/** Isolated real-broker load fixture; native session API is simulated. */
import { mkdtemp, chmod, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { Database } from "bun:sqlite";
import AgentBridge from "../plugins/opencode/index.ts";
import { brokerRpc } from "../plugins/opencode/rpc.ts";

const binary = process.argv[2];
if (!binary) throw new Error("Usage: bun tools/opencode-overload.ts /absolute/path/to/agent-bridge");
const root = await mkdtemp(join(process.env.AGENT_BRIDGE_FIXTURE_DIR ?? "/tmp", "agb41-load-"));
await chmod(root, 0o700);
function startBroker() { return Bun.spawn([resolve(binary), "--state-dir", root, "serve", "--no-discovery"], { stdout: "ignore", stderr: "pipe" }); }
let broker = startBroker();
const logs = [new Response(broker.stderr).text()];
const cleanups: (() => Promise<void>)[] = [];
const sessions = Array.from({ length: 58 }, (_, i) => `ses_${String(i).padStart(26, "0")}`);
const infos = new Map(sessions.map(id => [id, { id, title: "Fixture", time: { created: 1, updated: 1 }, location: { directory: root } }]));
const prompts = new Map<string, any>();
let promptAttempts = 0;
async function waitFor<T>(f: () => Promise<T | undefined>, ms = 30000): Promise<T> {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) { try { const value = await f(); if (value) return value; } catch {} await Bun.sleep(50); }
  throw new Error("fixture wait timed out");
}
try {
  await waitFor(async () => await brokerRpc(root, { op: "status" }));
  const entry = AgentBridge as unknown as { setup: (context: any) => Promise<() => Promise<void>> };
  for (let instance = 0; instance < 8; instance++) {
    const ctx = {
      options: { stateDir: root }, location: { directory: root },
      session: {
        list: async () => ({ data: [...infos.values()], cursor: { next: null } }),
        get: async ({ sessionID }: any) => infos.get(sessionID),
        context: async ({ sessionID }: any) => [...prompts.values()].filter(v => v.sessionID === sessionID && !v.text.includes("unknown-fixture")).map(v => ({ id: v.id, type: "user", text: v.text })),
        prompt: async ({ sessionID, id, text }: any) => {
          promptAttempts++;
          if (prompts.has(id)) throw new Error("duplicate native submission");
          prompts.set(id, { sessionID, id, text });
          if (text.includes("unknown-fixture")) throw new Error("simulated lost native receipt");
          return { id, sessionID, time: { created: Date.now() }, type: "user", payload: { text }, delivery: "steer" };
        },
        hook: async () => ({ dispose() {} }),
      },
      event: { subscribe: ({ signal }: any) => ({ async *[Symbol.asyncIterator]() {
        for (const sessionID of sessions) yield { type: "session.created", data: { sessionID }, location: { directory: root } };
        await new Promise<void>(resolve => signal.addEventListener("abort", resolve, { once: true }));
      } }) },
    };
    cleanups.push(await entry.setup(ctx));
  }
  const poll = async () => {
    const result: any = await brokerRpc(root, { op: "list", all: false });
    const agents = Array.isArray(result) ? result : result.agents;
    return agents?.filter((a: any) => sessions.some(s => a.name === `opencode-${s}`)).length === 58 ? agents : undefined;
  };
  const statuses = await Promise.all(Array.from({ length: 20 }, () => brokerRpc(root, { op: "status" })));
  await waitFor(poll, 45000);
  function leaseSnapshot(): Map<string, number> {
    const db = new Database(join(root, "messages.sqlite3"), { readonly: true });
    try {
      const rows = db.query("SELECT session,expires FROM opencode_leases WHERE ready=1").all() as { session: string; expires: number }[];
      return new Map(rows.map(row => [row.session, row.expires]));
    } finally { db.close(); }
  }
  const before = leaseSnapshot();
  if (before.size !== 58) throw new Error(`expected 58 leases, found ${before.size}`);
  const id = randomUUID();
  await brokerRpc(root, { op: "send", id, sender: "user", recipient: `opencode-${sessions[0]}`, body: "isolated test", parent: null });
  const delivered: any = await waitFor(async () => { const value: any = await brokerRpc(root, { op: "get", id }); return value.state === "delivered" ? value : undefined; }, 20000);
  const unknownId = randomUUID();
  await brokerRpc(root, { op: "send", id: unknownId, sender: "user", recipient: `opencode-${sessions[0]}`, body: "unknown-fixture", parent: null });
  await waitFor(async () => { const value: any = await brokerRpc(root, { op: "get", id: unknownId }); return value.state === "unknown" ? value : undefined; }, 20000);
  const laterId = randomUUID();
  await brokerRpc(root, { op: "send", id: laterId, sender: "user", recipient: `opencode-${sessions[0]}`, body: "later input", parent: null });
  await waitFor(async () => { const value: any = await brokerRpc(root, { op: "get", id: laterId }); return value.state === "delivered" ? value : undefined; }, 20000);
  const after = await waitFor(async () => {
    const current = leaseSnapshot();
    return current.size === 58 && sessions.every(session => (current.get(session) ?? 0) > (before.get(session) ?? 0)) ? current : undefined;
  }, 15000);
  broker.kill(); await broker.exited;
  broker = startBroker(); logs.push(new Response(broker.stderr).text());
  await waitFor(async () => await brokerRpc(root, { op: "status" }));
  await waitFor(poll, 45000);
  const restarted = await waitFor(async () => {
    const current = leaseSnapshot();
    return current.size === 58 && sessions.every(session => (current.get(session) ?? 0) > (after.get(session) ?? 0)) ? current : undefined;
  }, 25000);
  const afterRestart: any = await brokerRpc(root, { op: "get", id });
  const unknownAfterRestart: any = await brokerRpc(root, { op: "get", id: unknownId });
  if (prompts.size !== 3 || promptAttempts !== 3 || delivered.state !== "delivered" || afterRestart.state !== "delivered" || unknownAfterRestart.state !== "unknown") throw new Error(`unexpected prompts=${prompts.size} attempts=${promptAttempts} unknown=${unknownAfterRestart.state}`);
  console.log(JSON.stringify({ result: "passed", instances: 8, sessions: 58, renewed_leases: after.size, renewed_after_restart: restarted.size, status_requests: statuses.length, native_prompts: prompts.size, native_prompt_attempts: promptAttempts, unknown_replayed: false, later_delivered: true }));
} finally {
  await Promise.allSettled(cleanups.map(cleanup => cleanup()));
  broker.kill(); await broker.exited;
  const output = (await Promise.all(logs)).join("\n");
  if (broker.exitCode !== 0 && broker.exitCode !== 143 && broker.exitCode !== null) console.error(output);
  await rm(root, { recursive: true, force: true });
}
