import { afterEach, describe, expect, test } from "bun:test";
import { createOpencodeClient } from "@opencode-ai/sdk";
import type { PluginInput } from "@opencode-ai/plugin";
import { chmod, mkdtemp, rm, stat, writeFile, readFile, symlink } from "node:fs/promises";
import { join } from "node:path";
import { connect } from "node:net";
import { randomUUID } from "node:crypto";
import { Database } from "bun:sqlite";
import { Journal, nativeID, MAX_JOURNAL_ROWS, MAX_JOURNAL_BYTES } from "./journal.ts";
import AgentBridge from "./index.ts";
import { sameUser } from "./peer.ts";

const A = "ses_0123456789abcdefghijklmnop";
const B = "ses_ABCDEFGHIJKLMNOP0123456789";
const ID = "12345678-1234-4234-8234-123456789abc";
const NAMESPACE = "aabbccdd-1234-4234-8234-123456789abc";
const NATIVE = nativeID(NAMESPACE, A, ID);
const routes = new Map<string, { namespace: string; generation: string }>();
const cleanups: Array<() => Promise<void>> = [];
afterEach(async () => { for (const cleanup of cleanups.splice(0).reverse()) await cleanup(); });

async function frame(endpoint: string, value: unknown, raw = false, sent?: () => void): Promise<any> {
  if (!raw && value && typeof value === "object" && ["deliver", "reconcile"].includes((value as any).op)) {
    const f = value as any;
    value = { ...routes.get(`${endpoint}/${f.session}`), attempt: "aa345678-1234-4234-8234-123456789abc", native_message_id: nativeID(NAMESPACE, f.session, f.id), ...f };
  }
  return new Promise((resolve, reject) => {
    const socket = connect(endpoint);
    let data = "";
    socket.setTimeout(7000, () => { socket.destroy(); reject(new Error("Frame timeout")); });
    socket.on("error", reject);
    socket.on("connect", () => { socket.write(raw ? value as string : JSON.stringify(value) + "\n"); sent?.(); });
    socket.on("data", chunk => { data += chunk.toString(); if (data.includes("\n")) { socket.destroy(); resolve(JSON.parse(data.split("\n")[0]!)); } });
    socket.on("end", () => { if (!data.includes("\n")) reject(new Error("Closed without response")); });
  });
}

async function setup(tmpOverride?: string) {
  const directory = await mkdtemp("/tmp/aboc-test-");
  await chmod(directory, 0o700);
  const registrations = new Map<string, any>();
  const requests: any[] = [];
  let admissionDenied = false;
  let admissionHook: (() => void) | undefined;
  let admissionGate: { enter: () => void; release: Promise<void> } | undefined;
  function blockAdmission() {
    let enter!: () => void, release!: () => void;
    const entered = new Promise<void>(resolve => { enter = resolve; });
    const held = new Promise<void>(resolve => { release = resolve; });
    admissionGate = { enter, release: held };
    return { entered, release };
  }
  const listener = Bun.listen<{ buffer: string }>({ unix: join(directory, "bridge.sock"), socket: {
    open(socket) { socket.data = { buffer: "" }; },
    async data(socket, data) {
      socket.data.buffer += data.toString();
      if (!socket.data.buffer.includes("\n")) return;
      const request = JSON.parse(socket.data.buffer.split("\n")[0]!);
      requests.push(request);
      let result: any = {};
      if (request.op === "opencode_observe") {
        const old = registrations.get(request.session);
        const generation = old?.endpoint === request.endpoint ? old.generation : randomUUID();
        registrations.set(request.session, { ...request, generation });
        routes.set(`${request.endpoint}/${request.session}`, { generation, namespace: NAMESPACE });
        result = { generation, namespace: NAMESPACE, agent: {} };
      }
      if (request.op === "opencode_close" && registrations.get(request.session)?.generation === request.generation) registrations.delete(request.session);
      if (request.op === "opencode_heartbeat") {
        if (registrations.get(request.session)?.generation !== request.generation) { socket.end(JSON.stringify({ ok: false, error: "stale generation" }) + "\n"); return; }
        registrations.get(request.session).ready = request.ready;
      }
      if (request.op === "opencode_admit") {
        admissionHook?.();
        if (admissionGate) { const gate = admissionGate; admissionGate = undefined; gate.enter(); await gate.release; }
      }
      if (request.op === "opencode_admit" && (admissionDenied || registrations.get(request.session)?.generation !== request.generation)) { socket.end(JSON.stringify({ ok: false, error: "stale attempt" }) + "\n"); return; }
      socket.end(JSON.stringify({ ok: true, result }) + "\n");
    },
  }});
  await chmod(join(directory, "bridge.sock"), 0o600);
  const sessions = new Map<string, any>([A, B].map(id => [id, { id, projectID: "project", directory, title: id === A ? "Alpha" : "Beta", version: "1.18.35", time: { created: 1, updated: 1 } }]));
  const native = new Map<string, any>();
  const prompts: any[] = [];
  const statuses = new Map<string, any>();
  const sdkReads: string[] = [];
  let defer = false, wrongIdentity = false, lookupFailure = false;
  let blocked: { kind: "get" | "message" | "status" | "prompt"; skip: number; fail: boolean; enter: () => void; release: Promise<void>; returned: () => void } | undefined;
  function blockNext(kind: "get" | "message" | "status" | "prompt", skip = 0, fail = false) {
    let enter!: () => void, release!: () => void, returned!: () => void;
    const entered = new Promise<void>(resolve => { enter = resolve; });
    const blockedUntilRelease = new Promise<void>(resolve => { release = resolve; });
    const lookupReturned = new Promise<void>(resolve => { returned = resolve; });
    blocked = { kind, skip, fail, enter, release: blockedUntilRelease, returned };
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
    sdkReads.push(url.pathname);
    const statusSnapshot = Object.fromEntries(statuses);
    const kind = components[1] === "status" ? "status" : components.length === 2 ? "get" : components[2] === "message" ? "message" : components[2] === "prompt_async" ? "prompt" : undefined;
    if (blocked && blocked.kind === kind && blocked.skip-- === 0) {
      const gate = blocked;
      blocked = undefined;
      gate.enter();
      await gate.release;
      gate.returned();
      if (gate.fail) return Response.json({ error: "delayed lookup failure" }, { status: 500 });
    }
    if (components[1] === "status") return Response.json(statusSnapshot);
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
  const reload = async () => { await hooks.dispose?.(); hooks = await AgentBridge({ client, directory, worktree: directory } as PluginInput, { stateDir: directory }); };
  cleanups.push(async () => { try { await hooks.dispose?.(); } finally { listener.stop(true); await rm(directory, { recursive: true, force: true }); } });
  const event = async (type: string, properties: any) => hooks.event?.({ event: { type, properties } as any });
  const created = async (id = A) => event("session.created", { info: sessions.get(id) });
  const endpoint = () => registrations.get(A)?.endpoint ?? registrations.get(B)?.endpoint;
  const journalState = () => {
    const db = new Database(join(directory, "opencode-receipts.sqlite3"), { readonly: true });
    try { return db.query("SELECT namespace,id,session,state FROM receipts ORDER BY id").all(); } finally { db.close(); }
  };
  return { directory, registrations, requests, denyAdmission: (denied: boolean, hook?: () => void) => { admissionDenied = denied; admissionHook = hook; }, sessions, native, prompts, statuses, sdkReads, get hooks() { return hooks; }, reload, journalState, event, created, endpoint, blockNext, blockAdmission, defer: (v: boolean) => { defer = v; }, wrongIdentity: () => { wrongIdentity = true; }, lookupFailure: () => { lookupFailure = true; } };
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
    expect(reply.ok).toBe(false); expect(reply.status).toBe("uncertain");
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
  test("prepared crash window admits once while attempting crash stays uncertain after restart", async () => {
    const s = await setup(); await s.created();
    const journal = await Journal.open(s.directory);
    const identity = { namespace: NAMESPACE, session: A, id: ID, content: "prepared", native_message_id: NATIVE };
    journal.prepare(identity); journal.close();
    await s.reload(); await s.created();
    expect((await frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "prepared" })).status).toBe("inserted");
    expect(s.prompts).toHaveLength(1);
    const other = "22345678-1234-4234-8234-123456789abc";
    const crashed = { ...identity, id: other, content: "crash-before-insertion", native_message_id: nativeID(NAMESPACE, A, other) };
    const second = await Journal.open(s.directory); second.prepare(crashed); expect(second.attempt(crashed)).toBe(true); second.close();
    await s.reload(); await s.created();
    const before = s.journalState();
    expect((await frame(s.endpoint(), { op: "reconcile", session: A, id: other, content: crashed.content })).status).toBe("uncertain");
    expect(s.journalState()).toEqual(before);
    expect((await frame(s.endpoint(), { op: "deliver", session: A, id: other, content: crashed.content })).status).toBe("uncertain");
    expect(s.prompts).toHaveLength(1);
  });
  test("lost insertion receipt reconciles read-only after restart without another prompt", async () => {
    const s = await setup(); await s.created(); s.defer(true);
    const delivery = { op: "deliver", session: A, id: ID, content: "lost-receipt" };
    expect((await frame(s.endpoint(), delivery)).status).toBe("uncertain");
    expect(s.prompts).toHaveLength(1);
    s.native.set(`${A}/${NATIVE}`, { info: { id: NATIVE, sessionID: A, role: "user" }, parts: [{ type: "text", text: delivery.content }] });
    await s.reload(); await s.created();
    const before = s.journalState();
    expect((await frame(s.endpoint(), { ...delivery, op: "reconcile" })).status).toBe("inserted");
    expect(s.journalState()).toEqual(before);
    expect(s.prompts).toHaveLength(1);
  }, 10000);
  test("inserted tombstone persists across restart and changed payload is rejected", async () => {
    const s = await setup(); await s.created();
    const delivery = { op: "deliver", session: A, id: ID, content: "durable" };
    expect((await frame(s.endpoint(), delivery)).status).toBe("inserted");
    await s.reload(); await s.created();
    expect((await frame(s.endpoint(), { ...delivery, op: "reconcile" })).status).toBe("inserted");
    expect((await frame(s.endpoint(), { ...delivery, content: "mutated" })).ok).toBe(false);
    s.native.clear();
    expect((await frame(s.endpoint(), delivery)).status).toBe("uncertain");
    expect(s.prompts).toHaveLength(1);
    expect(s.journalState()).toEqual([{ namespace: NAMESPACE, id: ID, session: A, state: "inserted" }]);
  });
  test("reconcile with missing journal mapping neither creates rows nor prompts", async () => {
    const s = await setup(); await s.created();
    expect((await frame(s.endpoint(), { op: "reconcile", session: A, id: ID, content: "unknown" })).status).toBe("uncertain");
    expect(s.journalState()).toEqual([]); expect(s.prompts).toHaveLength(0);
    expect(s.requests.filter(request => request.op === "opencode_admit")).toHaveLength(0);
  });
  test("native busy and permission waits defer without future hidden submission", async () => {
    const s = await setup(); await s.created();
    const delivery = { op: "deliver", session: A, id: ID, content: "wait" };
    s.statuses.set(A, { type: "busy" });
    expect((await frame(s.endpoint(), delivery)).status).toBe("deferred");
    expect(s.journalState()).toEqual([]);
    s.statuses.delete(A);
    await s.event("permission.updated", { id: "permission1", sessionID: A });
    expect((await frame(s.endpoint(), delivery)).status).toBe("deferred");
    await s.event("permission.replied", { permissionID: "permission1", sessionID: A });
    await new Promise<void>(resolve => setImmediate(resolve));
    expect(s.prompts).toHaveLength(0);
    expect((await frame(s.endpoint(), delivery)).status).toBe("inserted");
    expect(s.prompts).toHaveLength(1);
  });
  test("old generation cannot deliver after reopening the same native session", async () => {
    const s = await setup(); await s.created();
    const old = routes.get(`${s.endpoint()}/${A}`)!;
    await s.event("session.deleted", { info: s.sessions.get(A) }); await s.created();
    expect((await frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "old", ...old })).ok).toBe(false);
    expect(s.prompts).toHaveLength(0);
  });
  test("independent journal handles permit only one durable attempt CAS", async () => {
    const s = await setup();
    const a = await Journal.open(s.directory), b = await Journal.open(s.directory);
    try {
      const identity = { namespace: NAMESPACE, session: A, id: ID, content: "CAS", native_message_id: NATIVE };
      a.prepare(identity); b.prepare(identity);
      expect([a.attempt(identity), b.attempt(identity)]).toEqual([true, false]);
      expect(() => b.prepare({ ...identity, session: B })).toThrow("mapping changed");
    } finally { a.close(); b.close(); }
  });
  test("simultaneous separate plugin processes initialize one private journal and elect one submitter", async () => {
    const directory = await mkdtemp("/tmp/aboc-journal-process-"); await chmod(directory, 0o700);
    try {
      const module = new URL("./journal.ts", import.meta.url).href;
      const identity = { namespace: NAMESPACE, session: A, id: ID, content: "process-CAS", native_message_id: NATIVE };
      const program = `import {Journal} from ${JSON.stringify(module)}; const j=await Journal.open(${JSON.stringify(directory)}); const i=${JSON.stringify(identity)}; j.prepare(i); console.log(j.attempt(i)); j.close();`;
      const children = [0, 1].map(() => Bun.spawn([process.execPath, "-e", program], { stdout: "pipe", stderr: "pipe" }));
      const result = await Promise.all(children.map(async child => ({ code: await child.exited, out: await new Response(child.stdout).text(), err: await new Response(child.stderr).text() })));
      expect(result.map(item => ({ code: item.code, err: item.err }))).toEqual([{ code: 0, err: "" }, { code: 0, err: "" }]);
      expect(result.map(item => item.out.trim()).sort()).toEqual(["false", "true"]);
      expect((await stat(join(directory, "opencode-receipts.sqlite3"))).mode & 0o777).toBe(0o600);
    } finally { await rm(directory, { recursive: true, force: true }); }
  }, 20000);
  test("journal row capacity preserves tombstones and known ID mappings", async () => {
    const s = await setup();
    const journal = await Journal.open(s.directory);
    const identity = { namespace: NAMESPACE, session: A, id: ID, content: "known", native_message_id: NATIVE };
    journal.prepare(identity); journal.inserted(identity);
    const db = new Database(join(s.directory, "opencode-receipts.sqlite3"));
    try {
      db.exec(`WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<${MAX_JOURNAL_ROWS-1}) INSERT INTO receipts SELECT 'capacity','id-'||n,'session','','native-'||n,'inserted' FROM x`);
      expect(() => journal.prepare({ ...identity, id: "new", native_message_id: "new" })).toThrow("capacity");
      expect(journal.prepare(identity).state).toBe("inserted");
      expect((db.query("SELECT count(*) AS n FROM receipts").get() as any).n).toBe(MAX_JOURNAL_ROWS);
    } finally { db.close(); journal.close(); }
  });
  test("journal UTF-8 byte capacity is durable across handles", async () => {
    const s = await setup();
    const db = new Database(join(s.directory, "opencode-receipts.sqlite3"));
    const journal = await Journal.open(s.directory);
    try {
      db.exec(`WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<512) INSERT INTO receipts SELECT 'capacity','id-'||n,'session',printf('%0*d',${MAX_JOURNAL_BYTES/512},0),'native-'||n,'inserted' FROM x`);
      expect(() => journal.prepare({ namespace: NAMESPACE, session: A, id: ID, content: "€", native_message_id: NATIVE })).toThrow("capacity");
      expect((db.query("SELECT sum(length(CAST(content AS BLOB))) AS n FROM receipts").get() as any).n).toBe(MAX_JOURNAL_BYTES);
    } finally { db.close(); journal.close(); }
  });
  test("corrupt or symlinked receipt journals fail closed without replacement", async () => {
    const directory = await mkdtemp("/tmp/aboc-corrupt-"); await chmod(directory, 0o700);
    const path = join(directory, "opencode-receipts.sqlite3");
    try {
      await writeFile(path, "not a receipt database", { mode: 0o600 });
      await expect(Journal.open(directory)).rejects.toThrow();
      expect(await readFile(path, "utf8")).toBe("not a receipt database");
      await rm(path);
      const target = join(directory, "private-target"); await writeFile(target, "untouched", { mode: 0o600 }); await symlink(target, path);
      await expect(Journal.open(directory)).rejects.toThrow("private, regular");
      expect(await readFile(target, "utf8")).toBe("untouched");
    } finally { await rm(directory, { recursive: true, force: true }); }
  });
  test("a busy event racing an idle status snapshot defers without a journal attempt", async () => {
    const s = await setup(); await s.created();
    const gate = s.blockNext("status");
    const delivery = frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "idle-race" });
    await gate.entered;
    await s.event("session.status", { sessionID: A, status: { type: "busy" } });
    gate.release(); await gate.settled();
    expect((await delivery).status).toBe("deferred");
    expect(s.prompts).toHaveLength(0); expect(s.journalState()).toEqual([]);
  });
  test("readiness heartbeats use cached events and expired leases reject before native lookup", async () => {
    const s = await setup(); await s.created();
    const reads = s.sdkReads.length;
    await s.event("session.status", { sessionID: A, status: { type: "busy" } });
    expect(s.sdkReads).toHaveLength(reads);
    expect(s.requests.filter(request => request.op === "opencode_heartbeat").at(-1).ready).toBe(false);
    const now = Date.now;
    try {
      Date.now = () => now() + 20001;
      expect((await frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "expired" })).ok).toBe(false);
    } finally { Date.now = now; }
    expect(s.sdkReads).toHaveLength(reads); expect(s.prompts).toHaveLength(0);
  });
  test("missing journal mappings reconcile exact existing native proof without mutation", async () => {
    const s = await setup(); await s.created();
    const legacy = "msg_" + ID.replaceAll("-", "");
    for (const native of [NATIVE, legacy]) {
      const request = { op: "reconcile", session: A, id: ID, content: "migrated exact proof", native_message_id: native };
      s.native.set(`${A}/${native}`, { info: { id: native, sessionID: A, role: "user" }, parts: [{ type: "text", text: request.content }] });
      const before = s.journalState();
      expect((await frame(s.endpoint(), request)).status).toBe("inserted");
      expect(s.journalState()).toEqual(before);
      expect(s.prompts).toHaveLength(0);
      expect(s.requests.filter(item => item.op === "opencode_admit")).toHaveLength(0);
      s.native.delete(`${A}/${native}`);
      expect((await frame(s.endpoint(), request)).status).toBe("uncertain");
      expect(s.journalState()).toEqual(before);
    }
  });
  test("legacy native IDs are reconcile-only and missing legacy mapping stays uncertain", async () => {
    const s = await setup(); await s.created();
    const legacy = { session: A, id: ID, content: "legacy", native_message_id: "msg_" + ID.replaceAll("-", "") };
    expect((await frame(s.endpoint(), { op: "reconcile", ...legacy })).status).toBe("uncertain");
    expect((await frame(s.endpoint(), { op: "deliver", ...legacy })).ok).toBe(false);
    expect(s.prompts).toHaveLength(0); expect(s.journalState()).toEqual([]);
  });
  for (const op of ["probe", "deliver", "reconcile"] as const) {
    test(`failed stale ${op} lookup cannot close a reopened generation`, async () => {
      const s = await setup(); await s.created();
      const old = routes.get(`${s.endpoint()}/${A}`)!;
      const gate = s.blockNext("get", 0, true);
      const request = op === "probe" ? { op, session: A } : { op, session: A, id: ID, content: "old-generation" };
      const pending = frame(s.endpoint(), request);
      await gate.entered;
      await s.event("session.deleted", { info: s.sessions.get(A) });
      await s.created();
      const replacement = s.registrations.get(A).generation;
      expect(replacement).not.toBe(old.generation);
      gate.release(); await gate.settled();
      expect((await pending).ok).toBe(false);
      expect(s.registrations.get(A).generation).toBe(replacement);
      expect((await frame(s.endpoint(), { op: "probe", session: A })).ok).toBe(true);
      expect(s.prompts).toHaveLength(0);
    });
  }
  test("denied broker admission defers only an untouched prepared attempt", async () => {
    const s = await setup(); await s.created(); s.denyAdmission(true);
    const delivery = { op: "deliver", session: A, id: ID, content: "paused-before-attempt" };
    const denied = await frame(s.endpoint(), delivery);
    expect(denied.status).toBe("deferred"); expect(denied.error).toContain("no native attempt");
    expect(s.prompts).toHaveLength(0);
    expect(s.journalState()).toEqual([{ namespace: NAMESPACE, id: ID, session: A, state: "prepared" }]);
    s.denyAdmission(false);
    await s.event("session.status", { sessionID: A, status: { type: "idle" } });
    expect(s.prompts).toHaveLength(0);
    expect((await frame(s.endpoint(), { ...delivery, attempt: randomUUID() })).status).toBe("inserted");
    expect(s.prompts).toHaveLength(1);
    s.native.clear(); s.denyAdmission(true);
    expect((await frame(s.endpoint(), delivery)).status).toBe("uncertain");
    expect(s.prompts).toHaveLength(1);
  });
  test("full durable journal explicitly defers new IDs before native attempt", async () => {
    const s = await setup(); await s.created();
    const db = new Database(join(s.directory, "opencode-receipts.sqlite3"));
    try {
      db.exec(`WITH RECURSIVE x(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM x WHERE n<${MAX_JOURNAL_ROWS}) INSERT INTO receipts SELECT 'capacity','id-'||n,'session','','native-'||n,'inserted' FROM x`);
      const result = await frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "over-capacity" });
      expect(result.status).toBe("deferred"); expect(result.error).toContain("capacity");
      expect(s.prompts).toHaveLength(0);
      expect((db.query("SELECT count(*) AS n FROM receipts").get() as any).n).toBe(MAX_JOURNAL_ROWS);
      expect(s.requests.filter(request => request.op === "opencode_admit")).toHaveLength(0);
      s.native.set(`${A}/${NATIVE}`, { info: { id: NATIVE, sessionID: A, role: "user" }, parts: [{ type: "text", text: "over-capacity" }] });
      const existing = await frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "over-capacity" });
      expect(existing.status).toBe("uncertain"); expect(existing.error).toContain("capacity");
      expect((await frame(s.endpoint(), { op: "reconcile", session: A, id: ID, content: "over-capacity" })).status).toBe("inserted");
      expect(s.prompts).toHaveLength(0);
    } finally { db.close(); }
  });
  test("an admission failure cannot defer a concurrent durable attempt winner", async () => {
    const s = await setup(); await s.created();
    const db = new Database(join(s.directory, "opencode-receipts.sqlite3"));
    try {
      s.denyAdmission(true, () => { db.query("UPDATE receipts SET state='attempting' WHERE namespace=? AND id=?").run(NAMESPACE, ID); });
      expect((await frame(s.endpoint(), { op: "deliver", session: A, id: ID, content: "concurrent-attempt" })).status).toBe("uncertain");
      expect(s.journalState()).toEqual([{ namespace: NAMESPACE, id: ID, session: A, state: "attempting" }]);
      expect(s.prompts).toHaveLength(0);
    } finally { db.close(); }
  });
  test("identical in-flight deliveries share admission and cannot defer beside a pending prompt", async () => {
    const s = await setup(); await s.created(); s.defer(true);
    const admission = s.blockAdmission();
    const native = s.blockNext("prompt");
    const request = { op: "deliver", session: A, id: ID, content: "coalesced-admission" };
    const first = frame(s.endpoint(), request);
    await admission.entered;
    let sent!: () => void;
    const sentToSocket = new Promise<void>(resolve => { sent = resolve; });
    const duplicate = frame(s.endpoint(), { ...request, id: ID.toUpperCase() }, false, sent);
    await sentToSocket;
    await new Promise<void>(resolve => setImmediate(resolve));
    expect(s.requests.filter(item => item.op === "opencode_admit")).toHaveLength(1);
    // A conflicting attempt/payload may not join this admitted operation.
    expect((await frame(s.endpoint(), { ...request, attempt: randomUUID() })).ok).toBe(false);
    expect((await frame(s.endpoint(), { ...request, content: "conflict" })).ok).toBe(false);
    admission.release();
    await native.entered;
    await s.event("session.status", { sessionID: A, status: { type: "busy" } });
    // The one prompt is already attempted; both duplicate deliveries must
    // observe the same insertion result rather than one returning deferred.
    s.native.set(`${A}/${NATIVE}`, { info: { id: NATIVE, sessionID: A, role: "user" }, parts: [{ type: "text", text: request.content }] });
    native.release(); await native.settled();
    const [a, b] = await Promise.all([first, duplicate]);
    expect(a.status).toBe("inserted"); expect(b).toEqual({ ...a, id: ID.toUpperCase() });
    expect(s.prompts).toHaveLength(1);
    expect(s.requests.filter(item => item.op === "opencode_admit")).toHaveLength(1);
  });
  for (const failure of [false, true]) {
    test(`delayed status ${failure ? "failure" : "busy event"} cannot defer a newly persisted attempt fence`, async () => {
      const s = await setup(); await s.created();
      const native = s.blockNext("status", 0, failure);
      const request = { op: "deliver", session: A, id: ID, content: "other-instance-attempt" };
      const pending = frame(s.endpoint(), request);
      await native.entered;
      const journal = await Journal.open(s.directory);
      try {
        const identity = { namespace: NAMESPACE, session: A, id: ID, content: request.content, native_message_id: NATIVE };
        journal.prepare(identity); journal.attempt(identity);
      } finally { journal.close(); }
      if (!failure) await s.event("session.status", { sessionID: A, status: { type: "busy" } });
      native.release(); await native.settled();
      expect((await pending).status).toBe("uncertain");
      expect(s.prompts).toHaveLength(0);
      expect(s.journalState()).toEqual([{ namespace: NAMESPACE, id: ID, session: A, state: "attempting" }]);
    });
  }
  test("Rust schema-migrated legacy producer frame reconciles over raw private transport", async () => {
    const s = await setup(); await s.created();
    // Rust compares this golden with the actual schema-migrated broker dispatch
    // and validates namespace/generation/attempt UUIDs before normalizing them.
    // Replace only the opaque current lease generation, never mapping fields.
    const golden = JSON.parse(await readFile(join(import.meta.dir, "../../tests/fixtures/opencode-migrated-frame.json"), "utf8"));
    golden.generation = s.registrations.get(A).generation;
    const before = s.journalState();
    s.native.set(`${A}/${golden.native_message_id}`, {
      info: { id: golden.native_message_id, sessionID: A, role: "user" },
      parts: [{ type: "text", text: golden.content }],
    });
    const inserted = await frame(s.endpoint(), JSON.stringify(golden) + "\n", true);
    expect(inserted.status).toBe("inserted");
    expect(inserted.id).toBe(golden.id);
    expect(inserted.native_message_id).toBe(golden.native_message_id);
    expect(s.prompts).toHaveLength(0);
    expect(s.requests.filter(request => request.op === "opencode_admit")).toHaveLength(0);
    expect(s.journalState()).toEqual(before);
    s.native.delete(`${A}/${golden.native_message_id}`);
    expect((await frame(s.endpoint(), JSON.stringify(golden) + "\n", true)).status).toBe("uncertain");
    expect(s.prompts).toHaveLength(0);
    expect(s.requests.filter(request => request.op === "opencode_admit")).toHaveLength(0);
    expect(s.journalState()).toEqual(before);
  });
  test("chat resume registers same session and native delete unregisters", async () => {
    const s = await setup();
    await s.hooks["chat.message"]?.({ sessionID: A }, { message: {} as any, parts: [] });
    expect(s.registrations.has(A)).toBe(true);
    await s.event("session.deleted", { info: s.sessions.get(A) });
    expect(s.registrations.has(A)).toBe(false);
  });
});
