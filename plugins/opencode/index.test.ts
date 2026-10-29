import { afterEach, describe, expect, test } from "bun:test";
import { createOpencodeClient } from "@opencode-ai/sdk";
import type { PluginInput } from "@opencode-ai/plugin";
import { chmod, mkdtemp, rm, stat } from "node:fs/promises";
import { join } from "node:path";
import { connect } from "node:net";
import AgentBridge from "./index.ts";
import { sameUser } from "./peer.ts";

const A = "ses_0123456789abcdefghijklmnop";
const B = "ses_ABCDEFGHIJKLMNOP0123456789";
const ID = "12345678-1234-4234-8234-123456789abc";
const NATIVE = "msg_" + ID.replaceAll("-", "");
const cleanups: Array<() => Promise<void>> = [];
afterEach(async () => { for (const cleanup of cleanups.splice(0).reverse()) await cleanup(); });

async function frame(endpoint: string, value: unknown, raw = false): Promise<any> {
  return new Promise((resolve, reject) => {
    const socket = connect(endpoint);
    let data = "";
    socket.setTimeout(7000, () => { socket.destroy(); reject(new Error("Frame timeout")); });
    socket.on("error", reject);
    socket.on("connect", () => socket.write(raw ? value as string : JSON.stringify(value) + "\n"));
    socket.on("data", chunk => { data += chunk.toString(); if (data.includes("\n")) { socket.destroy(); resolve(JSON.parse(data.split("\n")[0]!)); } });
    socket.on("end", () => { if (!data.includes("\n")) reject(new Error("Closed without response")); });
  });
}

async function setup(tmpOverride?: string) {
  const directory = await mkdtemp("/tmp/aboc-test-");
  await chmod(directory, 0o700);
  const registrations = new Map<string, any>();
  const requests: any[] = [];
  const listener = Bun.listen<{ buffer: string }>({ unix: join(directory, "bridge.sock"), socket: {
    open(socket) { socket.data = { buffer: "" }; },
    data(socket, data) {
      socket.data.buffer += data.toString();
      if (!socket.data.buffer.includes("\n")) return;
      const request = JSON.parse(socket.data.buffer.split("\n")[0]!);
      requests.push(request);
      if (request.op === "opencode_observe") registrations.set(request.session, request);
      if (request.op === "unregister") registrations.delete(request.session);
      socket.end(JSON.stringify({ ok: true, result: {} }) + "\n");
    },
  }});
  await chmod(join(directory, "bridge.sock"), 0o600);
  const sessions = new Map<string, any>([A, B].map(id => [id, { id, projectID: "project", directory, title: id === A ? "Alpha" : "Beta", version: "1.18.35", time: { created: 1, updated: 1 } }]));
  const native = new Map<string, any>();
  const prompts: any[] = [];
  let defer = false, wrongIdentity = false, lookupFailure = false;
  let blocked: { kind: "get" | "message"; skip: number; enter: () => void; release: Promise<void>; returned: () => void } | undefined;
  function blockNext(kind: "get" | "message", skip = 0) {
    let enter!: () => void, release!: () => void, returned!: () => void;
    const entered = new Promise<void>(resolve => { enter = resolve; });
    const blockedUntilRelease = new Promise<void>(resolve => { release = resolve; });
    const lookupReturned = new Promise<void>(resolve => { returned = resolve; });
    blocked = { kind, skip, enter, release: blockedUntilRelease, returned };
    return { entered, release, settled: async () => {
      await lookupReturned;
      // The fake fetch's SDK decoding and the handler continuations run as
      // microtasks. This event-loop barrier drains them before assertions.
      await new Promise<void>(resolve => setImmediate(resolve));
    }};
  }
  const client = createOpencodeClient({ baseUrl: "http://in-process-only.invalid", fetch: async (request) => {
    const req = request as Request;
    const url = new URL(req.url);
    const components = url.pathname.split("/").filter(Boolean);
    const id = components[1]!;
    if (components[0] !== "session") throw new Error("Unexpected SDK request");
    const kind = components.length === 2 ? "get" : components[2] === "message" ? "message" : undefined;
    if (blocked && blocked.kind === kind && blocked.skip-- === 0) {
      const gate = blocked;
      blocked = undefined;
      gate.enter();
      await gate.release;
      gate.returned();
    }
    if (components.length === 2) {
      const session = sessions.get(id);
      return Response.json(session ? { ...session, ...(wrongIdentity ? { id: B } : {}) } : { error: "missing" }, { status: session ? 200 : 404 });
    }
    if (components[2] === "prompt_async") {
      const body = await req.json();
      prompts.push({ session: id, body });
      if (!defer) native.set(`${id}/${body.messageID}`, { info: { id: body.messageID, sessionID: id, role: "user" }, parts: body.parts });
      return new Response(null, { status: 204 });
    }
    if (components[2] === "message") {
      if (lookupFailure) return Response.json({ error: "failure" }, { status: 500 });
      const found = native.get(`${id}/${components[3]}`);
      return Response.json(found ?? { error: "missing" }, { status: found ? 200 : 404 });
    }
    throw new Error("Unexpected SDK endpoint");
  }});
  const savedTmp = process.env.TMPDIR;
  if (tmpOverride) process.env.TMPDIR = tmpOverride;
  let hooks: Awaited<ReturnType<typeof AgentBridge>>;
  try { hooks = await AgentBridge({ client, directory, worktree: directory, project: { id: "project" }, serverUrl: new URL("http://127.0.0.1:1") } as PluginInput, { stateDir: directory });
  } finally { if (savedTmp === undefined) delete process.env.TMPDIR; else process.env.TMPDIR = savedTmp; }
  cleanups.push(async () => { try { await hooks.dispose?.(); } finally { listener.stop(true); await rm(directory, { recursive: true, force: true }); } });
  const event = async (type: string, properties: any) => hooks.event?.({ event: { type, properties } as any });
  const created = async (id = A) => event("session.created", { info: sessions.get(id) });
  const endpoint = () => registrations.get(A)?.endpoint ?? registrations.get(B)?.endpoint;
  return { directory, registrations, requests, sessions, native, prompts, hooks, event, created, endpoint, blockNext, defer: (v: boolean) => { defer = v; }, wrongIdentity: () => { wrongIdentity = true; }, lookupFailure: () => { lookupFailure = true; } };
}

describe("V1 supplied-client plugin with real private Unix sockets", () => {
  test("missing and invalid connected descriptors fail peer authentication", () => {
    expect(sameUser({})).toBe(false); expect(sameUser({ fd: -1 })).toBe(false); expect(sameUser({ fd: 99999999 })).toBe(false);
  });
  test("long macOS-style TMPDIR does not overflow portable Unix socket paths", async () => {
    const s = await setup("/var/folders/" + "a".repeat(120) + "/T");
    await s.created();
    expect(Buffer.byteLength(s.endpoint())).toBeLessThan(104);
    expect((await frame(s.endpoint(), { op: "probe", session: A })).ok).toBe(true);
  });
  test("has no archived-history discovery, verifies creation, selection, title and dispose", async () => {
    const s = await setup();
    expect(s.registrations.size).toBe(0);
    await s.created();
    await s.event("tui.session.select", { sessionID: B });
    expect([...s.registrations.keys()]).toEqual([A, B]);
    const endpoint = s.endpoint();
    expect((await stat(endpoint)).mode & 0o777).toBe(0o600);
    expect((await stat(join(endpoint, ".."))).mode & 0o777).toBe(0o700);
    s.sessions.get(A).title = "Renamed";
    await s.event("session.updated", { info: s.sessions.get(A) });
    expect(s.registrations.get(A).title).toBe("Renamed");
    expect(s.registrations.get(A).endpoint).toBe(endpoint);
    await s.hooks.dispose?.();
    expect(s.registrations.size).toBe(0);
    await expect(stat(endpoint)).rejects.toThrow();
  });
  test("unknown, deleted, wrong-client identity and archived sessions fail closed", async () => {
    const s = await setup();
    await s.created();
    expect((await frame(s.endpoint(), { op: "probe", session: B })).ok).toBe(false);
    s.sessions.delete(A);
    expect((await frame(s.endpoint(), { op: "probe", session: A })).ok).toBe(false);
    expect(s.prompts).toHaveLength(0);
    expect(s.registrations.has(A)).toBe(false);
  });
  test("rename of history is metadata only and cannot activate it", async () => {
    const s = await setup();
    await s.event("session.updated", { info: s.sessions.get(B) });
    expect(s.registrations.size).toBe(0);
    await s.created();
    s.wrongIdentity();
    expect((await frame(s.endpoint(), { op: "probe", session: A })).ok).toBe(false);
  });
  test("native ID map and concurrent/repeated ordinary retries insert only once into exact session", async () => {
    const s = await setup();
    await s.created();
    await s.created(B);
    const delivery = { op: "deliver", session: A, id: ID, content: "hello" };
    const results = await Promise.all([frame(s.endpoint(), delivery), frame(s.endpoint(), delivery), frame(s.endpoint(), { ...delivery, id: ID.toUpperCase() })]);
    expect(results[0].native_message_id).toBe(NATIVE);
    expect(results[1]).toEqual(results[0]);
    expect(results[2]).toEqual({ ...results[0], id: ID.toUpperCase() });
    expect((await frame(s.endpoint(), delivery))).toEqual(results[0]);
    expect(s.prompts).toEqual([{ session: A, body: { messageID: NATIVE, parts: [{ type: "text", text: "hello" }] } }]);
    expect((await frame(s.endpoint(), { ...delivery, session: B })).ok).toBe(false);
    expect((await frame(s.endpoint(), { ...delivery, content: "changed" })).ok).toBe(false);
    expect(s.native.has(`${B}/${NATIVE}`)).toBe(false);
  });
  test("204 is not insertion receipt and uncertain retries never resubmit", async () => {
    const s = await setup(); await s.created(); s.defer(true);
    const delivery = { op: "deliver", session: A, id: ID, content: "hello" };
    const reply = await frame(s.endpoint(), delivery);
    expect(reply.ok).toBe(false); expect(reply.uncertain).toBe(true);
    expect(s.prompts).toHaveLength(1);
    s.native.set(`${A}/${NATIVE}`, { info: { id: NATIVE, sessionID: A, role: "user" }, parts: [{ type: "text", text: "hello" }] });
    expect((await frame(s.endpoint(), delivery)).ok).toBe(true);
    expect(s.prompts).toHaveLength(1);
  }, 10000);
  test("rejects invalid bounded structured frames and permission/tool fields", async () => {
    const s = await setup(); await s.created();
    for (const request of [null, [], {}, { op: "probe", session: A, tools: {} }, { op: "probe", session: "build" }, { op: "deliver", session: A, id: ID, content: "" }, { op: "deliver", session: A, id: ID, content: "hello", permissions: [] }]) expect((await frame(s.endpoint(), request)).ok).toBe(false);
    expect((await frame(s.endpoint(), "{bad json}\n", true)).ok).toBe(false);
    expect((await frame(s.endpoint(), "x".repeat(256 * 1024 + 1), true)).ok).toBe(false);
    expect((await frame(s.endpoint(), '{}\n{}\n', true)).ok).toBe(false);
    expect(s.prompts).toHaveLength(0);
  });
  test("lookup failure never authorizes submission and archived sessions cannot receive", async () => {
    const s = await setup(); await s.created(); s.lookupFailure();
    const result = await frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "hello" });
    expect(result.ok).toBe(false); expect(s.prompts).toHaveLength(0);
    s.sessions.get(A).time.archived = 1;
    expect((await frame(s.endpoint(), { op: "probe", session: A })).ok).toBe(false);
    expect(s.registrations.has(A)).toBe(false);
  });
  test("receipt mismatch never reports submission", async () => {
    const s = await setup(); await s.created();
    s.native.set(`${A}/${NATIVE}`, { info: { id: NATIVE, sessionID: B, role: "user" }, parts: [{ type: "text", text: "hello" }] });
    expect((await frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "hello" })).ok).toBe(false);
    expect(s.prompts).toHaveLength(0);
  });
  test("archival update unregisters and broker outage preserves native chat", async () => {
    const s = await setup(); await s.created();
    s.sessions.get(A).time.archived = 1;
    await s.event("session.updated", { info: s.sessions.get(A) });
    expect(s.registrations.has(A)).toBe(false);
    // A missing exact session is an observation failure, not a native chat error.
    s.sessions.delete(B);
    await expect(s.hooks["chat.message"]!({ sessionID: B }, { message: {} as any, parts: [] })).resolves.toBeUndefined();
  });
  for (const kind of ["get", "message"] as const) {
    test(`dispose cancels delivery blocked in native ${kind} lookup`, async () => {
      const s = await setup(); await s.created();
      const gate = s.blockNext(kind);
      // Disposal deliberately closes the socket before the blocked native
      // lookup resumes. Consume that expected close, then await continuation.
      const delivery = frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "after-dispose" }).catch(() => undefined);
      await gate.entered;
      await s.hooks.dispose?.();
      expect(s.registrations.size).toBe(0);
      gate.release();
      await gate.settled();
      await delivery;
      expect(s.prompts).toHaveLength(0);
      expect(s.native.size).toBe(0);
      expect(s.registrations.size).toBe(0);
    });
    test(`native delete cancels delivery blocked in native ${kind} lookup`, async () => {
      const s = await setup(); await s.created();
      const gate = s.blockNext(kind);
      const delivery = frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "after-delete" });
      await gate.entered;
      await s.event("session.deleted", { info: s.sessions.get(A) });
      expect(s.registrations.size).toBe(0);
      gate.release();
      await gate.settled();
      expect((await delivery).ok).toBe(false);
      expect(s.prompts).toHaveLength(0);
      expect(s.native.size).toBe(0);
      expect(s.registrations.size).toBe(0);
    });
  }
  test("native deletion suppresses a late insertion receipt without rewriting history", async () => {
    const s = await setup(); await s.created();
    // Skip the initial native-ID absence lookup and block the receipt lookup
    // after promptAsync has already inserted the exact native user message.
    const gate = s.blockNext("message", 1);
    const delivery = frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "before-delete" });
    await gate.entered;
    expect(s.prompts).toHaveLength(1);
    expect(s.native.size).toBe(1);
    await s.event("session.deleted", { info: s.sessions.get(A) });
    gate.release();
    await gate.settled();
    expect((await delivery).ok).toBe(false);
    expect(s.registrations.size).toBe(0);
    expect(s.prompts).toHaveLength(1);
    expect(s.native.size).toBe(1);
  });
  test("dispose waits for a queued chat observation and leaves no registration", async () => {
    const s = await setup();
    const observation = s.hooks["chat.message"]!({ sessionID: A }, { message: {} as any, parts: [] });
    await s.hooks.dispose?.();
    await observation;
    expect(s.registrations.size).toBe(0);
  });
  test("chat resume registers same session and native delete unregisters", async () => {
    const s = await setup();
    await s.hooks["chat.message"]?.({ sessionID: A }, { message: {} as any, parts: [] });
    expect(s.registrations.has(A)).toBe(true);
    await s.event("session.deleted", { info: s.sessions.get(A) });
    expect(s.registrations.has(A)).toBe(false);
  });
});
