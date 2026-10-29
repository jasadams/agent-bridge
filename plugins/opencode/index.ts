import type { Plugin, Hooks } from "@opencode-ai/plugin";
import type { Session } from "@opencode-ai/sdk";
import { chmod, mkdtemp, rm } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";
import { createHash, randomUUID } from "node:crypto";
import { brokerRpc, MAX_FRAME } from "./rpc.ts";
import { sameUser } from "./peer.ts";

const SESSION = /^ses_[a-zA-Z0-9]{26}$/;
const UUID = /^[a-fA-F0-9]{8}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{12}$/;
const RECEIPT_MS = 3000;
const MAX_PENDING = 4096;
type Frame = { op: "probe"; session: string } | { op: "deliver"; session: string; id: string; content: string };
type Data = { buffer: Buffer; handled: boolean; timer?: ReturnType<typeof setTimeout> };
type Receipt = { ok: true; instance: string; session: string; id?: string; native_message_id?: string };
type Submission = { fingerprint: string; native: string; session: string; content: string; pending?: Promise<Receipt>; receipt?: Receipt };

function parseFrame(input: unknown): Frame {
  if (!input || typeof input !== "object" || Array.isArray(input)) throw new Error("Invalid frame");
  const f = input as Record<string, unknown>;
  const fields = f.op === "probe" ? ["op", "session"] : f.op === "deliver" ? ["op", "session", "id", "content"] : [];
  if (!fields.length || Object.keys(f).length !== fields.length || Object.keys(f).some(k => !fields.includes(k)) || typeof f.session !== "string" || !SESSION.test(f.session) || f.session.length > 200) throw new Error("Invalid frame identity");
  if (f.op === "deliver" && (typeof f.id !== "string" || !UUID.test(f.id) || typeof f.content !== "string" || !f.content.length || Buffer.byteLength(f.content) > MAX_FRAME - 1024 || f.content.includes("\0"))) throw new Error("Invalid delivery payload");
  return f as Frame;
}

// A V1 server plugin, using only its supplied SDK client. serverUrl may describe
// no listener at all: it is deliberately never used to construct another client.
const AgentBridge: Plugin = async (input, options = {}) => {
  const state = options.stateDir ?? process.env.AGENT_BRIDGE_STATE_DIR;
  if (typeof state !== "string" || !isAbsolute(state)) throw new Error("Agent Bridge plugin requires an absolute stateDir option or AGENT_BRIDGE_STATE_DIR");
  const instance = randomUUID();
  // macOS TMPDIR commonly exceeds sockaddr_un limits once UUID.sock is added.
  // /tmp is shared, but mkdtemp atomically creates this owner-private directory.
  const directory = await mkdtemp("/tmp/aboc-");
  await chmod(directory, 0o700);
  const endpoint = join(directory, instance + ".sock");
  const live = new Set<string>();
  const submissions = new Map<string, Submission>();
  const connections = new Set<Bun.Socket<Data>>();
  let disposed = false;
  let events = Promise.resolve();
  const name = (session: string) => `opencode-${session}`;

  function requireLive(session: string) {
    if (disposed || !live.has(session)) throw new Error("Unknown, stale or disposed session");
  }
  async function verified(session: string): Promise<Session> {
    if (disposed || !SESSION.test(session) || session.length > 200) throw new Error("Invalid or disposed session");
    const result = await input.client.session.get({ path: { id: session } });
    if (disposed) throw new Error("Plugin disposed during session lookup");
    if (result.error || !result.data || result.data.id !== session || resolve(result.data.directory) !== resolve(input.directory)) throw new Error("Session does not belong to this plugin instance");
    const time = result.data.time as Session["time"] & { archived?: number };
    if (time.archived) throw new Error("Session is archived");
    return result.data;
  }
  async function unregister(session: string) {
    live.delete(session);
    await brokerRpc(state as string, { op: "unregister", name: name(session), session });
  }
  async function observe(session: string) {
    let info: Session;
    try { info = await verified(session); }
    catch (error) { if (live.has(session)) await unregister(session); throw error; }
    live.add(session); // Probe during registration must see this exact identity.
    try {
      await brokerRpc(state as string, { op: "opencode_observe", session, endpoint, title: info.title });
    } catch (error) { live.delete(session); throw error; }
  }
  async function receipt(frame: Extract<Frame, { op: "deliver" }>, item: Submission): Promise<Receipt> {
    const deadline = Date.now() + RECEIPT_MS;
    do {
      requireLive(frame.session);
      const result = await input.client.session.message({ path: { id: frame.session, messageID: item.native } });
      requireLive(frame.session);
      if (result.data) {
        const { info, parts } = result.data;
        const text = parts.filter(p => p.type === "text").map(p => p.text).join("");
        if (info.id !== item.native || info.sessionID !== frame.session || info.role !== "user" || text !== item.content) throw new Error("Native message identity or payload mismatch");
        const reply: Receipt = { ok: true, instance, session: frame.session, id: frame.id, native_message_id: item.native };
        item.receipt = reply;
        return reply;
      }
      await Bun.sleep(25);
    } while (Date.now() < deadline && !disposed);
    throw new Error("Native insertion is unconfirmed; API acceptance is not a receipt");
  }
  async function handle(frame: Frame): Promise<Receipt> {
    requireLive(frame.session);
    try { await verified(frame.session); }
    catch (error) { if (live.has(frame.session)) await unregister(frame.session); throw error; }
    requireLive(frame.session);
    if (frame.op === "probe") return { ok: true, instance, session: frame.session };
    const fingerprint = createHash("sha256").update(frame.session).update("\0").update(frame.content).digest("hex");
    let item = submissions.get(frame.id.toLowerCase());
    if (item) {
      if (item.fingerprint !== fingerprint) throw new Error("Broker message ID reused for a different payload");
      if (item.receipt) return { ...item.receipt, id: frame.id };
      if (!item.pending) item.pending = receipt(frame, item).finally(() => { item!.pending = undefined; });
      return item.pending.then(result => ({ ...result, id: frame.id }));
    }
    if (submissions.size >= MAX_PENDING) throw new Error("Plugin receipt capacity reached; restart recovery is unsupported");
    item = { fingerprint, native: `msg_${frame.id.replaceAll("-", "").toLowerCase()}`, session: frame.session, content: frame.content };
    submissions.set(frame.id.toLowerCase(), item);
    const submission = item;
    item.pending = (async () => {
      // Check for the stable native ID before submitting, without rewriting it.
      const existing = await input.client.session.message({ path: { id: frame.session, messageID: submission.native } });
      requireLive(frame.session);
      if (!existing.data) {
        if (existing.response.status !== 404) throw new Error("Cannot establish native message absence");
        requireLive(frame.session);
        const result = await input.client.session.promptAsync({ path: { id: frame.session }, body: { messageID: submission.native, parts: [{ type: "text", text: frame.content }] } });
        requireLive(frame.session);
        if (result.error) throw new Error("Native prompt submission failed; insertion is unconfirmed");
      }
      return receipt(frame, submission);
    })().finally(() => { submission.pending = undefined; });
    return item.pending.then(result => ({ ...result, id: frame.id }));
  }

  const listener = Bun.listen<Data>({ unix: endpoint, socket: {
    open(socket) {
      if (disposed || !sameUser(socket)) { socket.terminate(); return; }
      socket.data = { buffer: Buffer.alloc(0), handled: false };
      connections.add(socket);
      socket.data.timer = setTimeout(() => socket.terminate(), 6000);
    },
    data(socket, chunk) {
      const data = socket.data;
      if (!data || data.handled) { socket.terminate(); return; }
      data.buffer = Buffer.concat([data.buffer, chunk]);
      if (data.buffer.length > MAX_FRAME) { socket.end(JSON.stringify({ ok: false, instance, error: "Frame exceeds limit" }) + "\n"); return; }
      const newline = data.buffer.indexOf(10);
      if (newline < 0) return;
      data.handled = true;
      if (newline !== data.buffer.length - 1) { socket.end(JSON.stringify({ ok: false, instance, error: "Expected exactly one frame" }) + "\n"); return; }
      let frame: Frame;
      try { frame = parseFrame(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(data.buffer.subarray(0, newline)))); }
      catch { socket.end(JSON.stringify({ ok: false, instance, error: "Invalid frame" }) + "\n"); return; }
      void handle(frame).then(
        result => socket.end(JSON.stringify(result) + "\n"),
        () => socket.end(JSON.stringify({ ok: false, instance, session: frame.session, ...(frame.op === "deliver" ? { id: frame.id, uncertain: submissions.has(frame.id.toLowerCase()) } : {}), error: "Session unavailable or native insertion unconfirmed" }) + "\n"),
      );
    },
    close(socket) { if (socket.data?.timer) clearTimeout(socket.data.timer); connections.delete(socket); },
    error(socket) { socket.terminate(); },
  }});
  try { await chmod(endpoint, 0o600); }
  catch (error) { listener.stop(true); await rm(directory, { recursive: true, force: true }); throw error; }
  const hooks: Hooks = {
    event: async ({ event }) => {
      // v1.18.35 publishes tui.session.select but its generated V1 SDK union
      // omits it. Validate the runtime frame instead of using the V2 SDK.
      const value = event as unknown as { type: string; properties: Record<string, unknown> };
      const work = async () => {
        if (disposed) return;
        if (value.type === "session.deleted") {
          const info = value.properties.info as { id?: unknown } | undefined;
          if (typeof info?.id === "string" && live.has(info.id)) await unregister(info.id);
        } else if (value.type === "session.created" || value.type === "session.updated") {
          const info = value.properties.info as { id?: unknown } | undefined;
          if (typeof info?.id === "string" && (value.type === "session.created" || live.has(info.id))) await observe(info.id);
        } else if (["tui.session.select", "session.status"].includes(value.type) && typeof value.properties.sessionID === "string") {
          await observe(value.properties.sessionID);
        }
      };
      events = events.catch(() => {}).then(work);
      // Native dispatch does not await event hooks, so absorb errors here. Failed
      // observations remove live membership and cannot enable a destination.
      await events.catch(() => { console.error("Agent Bridge: native session registration is unavailable"); });
    },
    "chat.message": async ({ sessionID }) => {
      events = events.catch(() => {}).then(async () => { if (!disposed) await observe(sessionID); });
      await events.catch(() => { console.error("Agent Bridge: native session registration is unavailable"); });
    },
    dispose: async () => {
      if (disposed) return;
      disposed = true;
      listener.stop(true);
      for (const socket of connections) socket.terminate();
      await events.catch(() => {});
      const results = await Promise.allSettled([...live].map(unregister));
      await rm(directory, { recursive: true, force: true });
      if (results.some(result => result.status === "rejected")) throw new Error("Some OpenCode registrations could not be removed; their dead endpoint rejects delivery");
    },
  };
  return hooks;
};
export default AgentBridge;
