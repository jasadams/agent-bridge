import { connect, type Socket } from "node:net";
import { lstat, realpath } from "node:fs/promises";
import { dirname, isAbsolute, join } from "node:path";
import { sameUser } from "./peer.ts";

export const MAX_FRAME = 256 * 1024;
export async function privateSocket(path: string): Promise<void> {
  if (!isAbsolute(path)) throw new Error("Socket path must be absolute");
  const meta = await lstat(path), parent = await lstat(await realpath(dirname(path)));
  if (!meta.isSocket() || meta.uid !== process.getuid!() || (meta.mode & 0o077) || !parent.isDirectory() || parent.uid !== process.getuid!() || (parent.mode & 0o077)) throw new Error("Socket and directory must be private to this user");
}
export async function brokerRpc(state: string, request: unknown): Promise<unknown> {
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
