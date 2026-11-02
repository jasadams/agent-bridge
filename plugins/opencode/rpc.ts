import { connect, type Socket } from "node:net";
import { lstat, realpath } from "node:fs/promises";
import { dirname, isAbsolute, join } from "node:path";
import { sameUser } from "./peer.ts";

export const MAX_FRAME = 256 * 1024;
export class RpcNotSubmittedError extends Error {}
// Shared by plugin instances in this runtime. Keep lifecycle traffic from
// consuming every socket while preserving slots for admission and renewals.
const RPC_LIMIT = 32, OBSERVE_LIMIT = 12, HEARTBEAT_LIMIT = 8, CLOSE_LIMIT = 4, ADMISSION_LIMIT = 4, OTHER_LIMIT = 4, WAIT_LIMIT = 608, OBSERVE_WAIT_LIMIT = 384, HEARTBEAT_WAIT_LIMIT = 96, CLOSE_WAIT_LIMIT = 64, ADMISSION_WAIT_LIMIT = 32, OTHER_WAIT_LIMIT = 32, WAIT_MS = 4000;
type Kind = "observation" | "heartbeat" | "close" | "admission" | "other";
type Waiter = { kind: Kind; start: () => void; timer: ReturnType<typeof setTimeout> };
type Scheduler = { running: number; observations: number; heartbeats: number; closes: number; admissions: number; others: number; waiting: Waiter[] };
const globalScheduler = globalThis as typeof globalThis & { [key: symbol]: unknown };
const schedulerKey = Symbol.for("agent-bridge.opencode.rpc-scheduler.v1");
const scheduler = (globalScheduler[schedulerKey] ??= { running: 0, observations: 0, heartbeats: 0, closes: 0, admissions: 0, others: 0, waiting: [] }) as Scheduler;
const waiting = scheduler.waiting;
function pump() {
  while (scheduler.running < RPC_LIMIT) {
    const index = waiting.findIndex(item => item.kind === "other" && scheduler.others < OTHER_LIMIT || item.kind === "admission" && scheduler.admissions < ADMISSION_LIMIT || item.kind === "close" && scheduler.closes < CLOSE_LIMIT || item.kind === "observation" && scheduler.observations < OBSERVE_LIMIT || item.kind === "heartbeat" && scheduler.heartbeats < HEARTBEAT_LIMIT);
    if (index < 0) return;
    const item = waiting.splice(index, 1)[0]!;
    clearTimeout(item.timer);
    scheduler.running++;
    if (item.kind === "observation") scheduler.observations++;
    if (item.kind === "heartbeat") scheduler.heartbeats++;
    if (item.kind === "close") scheduler.closes++;
    if (item.kind === "admission") scheduler.admissions++;
    if (item.kind === "other") scheduler.others++;
    item.start();
  }
}
function slot(kind: Kind): Promise<() => void> {
  const sameKind = waiting.filter(item => item.kind === kind).length;
  if (waiting.length >= WAIT_LIMIT || kind === "observation" && sameKind >= OBSERVE_WAIT_LIMIT || kind === "heartbeat" && sameKind >= HEARTBEAT_WAIT_LIMIT || kind === "close" && sameKind >= CLOSE_WAIT_LIMIT || kind === "admission" && sameKind >= ADMISSION_WAIT_LIMIT || kind === "other" && sameKind >= OTHER_WAIT_LIMIT) return Promise.reject(new RpcNotSubmittedError(`Broker RPC capacity exhausted before submission (active=${scheduler.running}, queued=${waiting.length})`));
  return new Promise((resolve, reject) => {
    const item: Waiter = {
      kind,
      start: () => resolve(() => { scheduler.running--; if (kind === "observation") scheduler.observations--; if (kind === "heartbeat") scheduler.heartbeats--; if (kind === "close") scheduler.closes--; if (kind === "admission") scheduler.admissions--; if (kind === "other") scheduler.others--; pump(); }),
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
  const release = await slot(op === "opencode_observe" ? "observation" : op === "opencode_heartbeat" ? "heartbeat" : op === "opencode_close" ? "close" : op === "opencode_admit" ? "admission" : "other");
  try {
    return await performRpc(state, request);
  } finally { release(); }
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
