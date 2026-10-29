import { dlopen, FFIType, ptr } from "bun:ffi";

// Bun 1.3.11 exposes fd at runtime but omits it from the Socket declaration.
// Reject runtime changes instead of bypassing connected-peer authentication.
const libc = process.platform === "linux"
  ? dlopen("libc.so.6", { getsockopt: { args: [FFIType.i32, FFIType.i32, FFIType.i32, FFIType.ptr, FFIType.ptr], returns: FFIType.i32 } })
  : process.platform === "darwin"
    ? dlopen("/usr/lib/libSystem.B.dylib", { getpeereid: { args: [FFIType.i32, FFIType.ptr, FFIType.ptr], returns: FFIType.i32 } })
    : undefined;

export function sameUser(socket: unknown): boolean {
  const fd = (socket as { fd?: unknown }).fd;
  if (!libc || typeof fd !== "number" || !Number.isInteger(fd) || fd < 0 || !process.getuid) return false;
  if (process.platform === "linux") {
    const cred = new Int32Array(3);
    const size = new Uint32Array([cred.byteLength]);
    const call = "getsockopt" in libc.symbols ? libc.symbols.getsockopt : undefined;
    return !!call && call(fd, 1, 17, ptr(cred), ptr(size)) === 0 && size[0] === 12 && cred[1] === process.getuid();
  }
  const uid = new Uint32Array(1), gid = new Uint32Array(1);
  const call = "getpeereid" in libc.symbols ? libc.symbols.getpeereid : undefined;
  return !!call && call(fd, ptr(uid), ptr(gid)) === 0 && uid[0] === process.getuid();
}
