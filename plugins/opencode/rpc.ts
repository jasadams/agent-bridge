import { connect, type Socket } from "node:net";
import { lstat, realpath } from "node:fs/promises";
import { dirname, isAbsolute, join } from "node:path";
import { sameUser } from "./peer.ts";

export const MAX_FRAME = 256 * 1024;
export class RpcNotSubmittedError extends Error {}
// Shared by plugin instances in this runtime. Keep admission from being
// starved by other broker traffic.
const RPC_LIMIT = 32, ADMISSION_LIMIT = 4, OTHER_LIMIT = 4, WAIT_LIMIT = 64, ADMISSION_WAIT_LIMIT = 32, OTHER_WAIT_LIMIT = 32, WAIT_MS = 4000;
type Kind = "admission" | "other";
type Waiter = { kind: Kind; start: () => void; timer: ReturnType<typeof setTimeout> };
type Scheduler = { running: number; admissions: number; others: number; waiting: Waiter[] };
const globalScheduler = globalThis as typeof globalThis & { [key: symbol]: unknown };
const schedulerKey = Symbol.for("agent-bridge.opencode.rpc-scheduler.v2");
const scheduler = (globalScheduler[schedulerKey] ??= { running: 0, admissions: 0, others: 0, waiting: [] }) as Scheduler;
const waiting = scheduler.waiting;
function pump() {
  while (scheduler.running < RPC_LIMIT) {
    const index = waiting.findIndex(item => item.kind === "other" ? scheduler.others < OTHER_LIMIT : scheduler.admissions < ADMISSION_LIMIT);
    if (index < 0) return;
    const item = waiting.splice(index, 1)[0]!;
    clearTimeout(item.timer);
    scheduler.running++;
    if (item.kind === "admission") scheduler.admissions++; else scheduler.others++;
    item.start();
  }
}
function slot(kind: Kind): Promise<() => void> {
  const sameKind = waiting.filter(item => item.kind === kind).length;
  if (waiting.length >= WAIT_LIMIT || sameKind >= (kind === "admission" ? ADMISSION_WAIT_LIMIT : OTHER_WAIT_LIMIT)) return Promise.reject(new RpcNotSubmittedError(`Broker RPC capacity exhausted before submission (active=${scheduler.running}, queued=${waiting.length})`));
  return new Promise((resolve, reject) => {
    const item: Waiter = {
      kind,
      start: () => resolve(() => { scheduler.running--; if (kind === "admission") scheduler.admissions--; else scheduler.others--; pump(); }),
      timer: setTimeout(() => {
        const index = waiting.indexOf(item);
        if (index >= 0) waiting.splice(index, 1);
        reject(new RpcNotSubmittedError(`Broker RPC admission timed out before submission (active=${scheduler.running}, queued=${waiting.length})`));
      }, WAIT_MS),
    };
    waiting.push(item);
    pump();
  });
}
export async function privateSocket(path: string): Promise<void> {
  if (!isAbsolute(path)) throw new Error("Socket path must be absolute");
  const meta = await lstat(path), parent = await lstat(await realpath(dirname(path)));
  if (!meta.isSocket() || meta.uid !== process.getuid!() || (meta.mode & 0o077) || !parent.isDirectory() || parent.uid !== process.getuid!() || (parent.mode & 0o077)) throw new Error("Socket and directory must be private to this user");
}
export async function brokerRpc(state: string, request: unknown): Promise<unknown> {
  const op = !!request && typeof request === "object" ? (request as { op?: unknown }).op : undefined;
  const proceed = await slot(op === "opencode_admit" ? "admission" : "other");
  try {
    return await performRpc(state, request);
  } finally { proceed(); }
}
async function performRpc(state: string, request: unknown): Promise<unknown> {
  const path = join(state, "bridge.sock");
  await privateSocket(path);
  return new Promise((resolve, reject) => {
    const socket = connect(path);
    const fail = (error: Error) => { socket.destroy(); reject(error); };
    let data = Buffer.alloc(0);
    socket.setTimeout(5000, () => fail(new Error("Broker RPC timed out")));
    socket.on("error", fail);
    socket.on("connect", () => {
      // Node compatibility Socket wraps Bun's actual socket at _handle.
      const native = (socket as Socket & { _handle?: unknown })._handle;
      if (!sameUser(native)) return fail(new Error("Broker connected peer is not this user"));
      socket.write(JSON.stringify(request) + "\n");
    });
    socket.on("data", (chunk: Buffer) => {
      data = Buffer.concat([data, chunk]);
      if (data.length > MAX_FRAME) return fail(new Error("Broker response exceeds frame limit"));
      const newline = data.indexOf(10);
      if (newline < 0) return;
      try {
        const response = JSON.parse(data.subarray(0, newline).toString("utf8"));
        socket.destroy();
        if (response.ok !== true) reject(new Error(response.error ?? "Broker RPC failed"));
        else resolve(response.result);
      } catch (error) { fail(error as Error); }
    });
    socket.on("end", () => { if (data.indexOf(10) < 0) fail(new Error("Truncated broker response")); });
  });
}
