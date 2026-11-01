import { Database } from "bun:sqlite";
import { constants } from "node:fs";
import { lstat, open, realpath } from "node:fs/promises";
import { join } from "node:path";
import { createHash } from "node:crypto";

export type Identity = { namespace: string; id: string; session: string; content: string; native_message_id: string };
export type AttemptIdentity = Identity & { generation: string; attempt: string };
type Acceptance = { id: string; sessionID: string; type: "user"; payload: { text: string }; delivery: "steer"; time: { created: number } };
export function validateAcceptance(identity: Identity, value: unknown): Acceptance {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Malformed native acceptance receipt");
  const r = value as Record<string, any>;
  if (r.id !== identity.native_message_id || r.sessionID !== identity.session || r.type !== "user" || r.delivery !== "steer" || !r.payload || typeof r.payload !== "object" || Array.isArray(r.payload) || r.payload.text !== identity.content || !r.time || typeof r.time.created !== "number" || !Number.isFinite(r.time.created)) throw new Error("Native acceptance identity or content mismatch");
  for (const key of ["files", "agents", "skills"]) if (r.payload[key] !== undefined && (!Array.isArray(r.payload[key]) || r.payload[key].length)) throw new Error("Native acceptance contains unexpected attachments");
  return r as Acceptance;
}
export class JournalCapacityError extends Error {}
export const MAX_JOURNAL_ROWS = 65536;
export const MAX_JOURNAL_BYTES = 64 * 1024 * 1024;
type Row = Identity & { state: "prepared" | "attempting" | "inserted"; acceptance: string | null };
export function nativeID(namespace: string, session: string, id: string): string {
  return "msg_" + createHash("sha256").update(namespace).update("\0").update(session).update("\0").update(id.toLowerCase()).digest("hex");
}
export class Journal {
  private constructor(private db: Database) {}
  static async open(state: string): Promise<Journal> {
    const directory = await realpath(state), dir = await lstat(directory);
    if (!dir.isDirectory() || dir.uid !== process.getuid!() || (dir.mode & 0o077)) throw new Error("Journal state directory must be owner-private");
    const path = join(directory, "opencode-receipts.sqlite3");
    try { const file = await open(path, constants.O_CREAT | constants.O_EXCL | constants.O_RDWR | constants.O_NOFOLLOW, 0o600); await file.close(); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error; }
    const meta = await lstat(path);
    if (!meta.isFile() || meta.nlink !== 1 || meta.uid !== process.getuid!() || (meta.mode & 0o077)) throw new Error("Journal file must be private, regular and unlinked elsewhere");
    const db = new Database(path, { strict: true });
    try {
      db.exec("PRAGMA busy_timeout=1000; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;");
      if ((db.query("PRAGMA quick_check").get() as { quick_check: string }).quick_check !== "ok") throw new Error("Corrupt receipt journal");
      db.exec("BEGIN IMMEDIATE");
      const version = (db.query("PRAGMA user_version").get() as { user_version: number }).user_version;
      if (version !== 0 && version !== 1 && version !== 2) throw new Error("Unsupported receipt journal schema");
      if (version === 0) {
        if ((db.query("SELECT count(*) AS n FROM sqlite_master WHERE type='table'").get() as { n: number }).n !== 0) throw new Error("Unrecognized receipt journal");
        db.exec("CREATE TABLE receipts(namespace TEXT NOT NULL,id TEXT NOT NULL,session TEXT NOT NULL,content TEXT NOT NULL,native_message_id TEXT NOT NULL UNIQUE,state TEXT NOT NULL CHECK(state IN ('prepared','attempting','inserted')),PRIMARY KEY(namespace,id)) STRICT; PRAGMA user_version=1;");
      }
      // Historical inserted tombstones prove only their original transcript
      // readback. Never fabricate a native admission receipt for old rows.
      if (version < 2) db.exec("ALTER TABLE receipts ADD COLUMN acceptance TEXT; PRAGMA user_version=2;");
      db.query("SELECT namespace,id,session,content,native_message_id,state FROM receipts LIMIT 0").all();
      db.exec("COMMIT");
      return new Journal(db);
    } catch (error) { db.close(); throw error; }
  }
  find(identity: Identity): Row | undefined {
    const row = this.db.query("SELECT * FROM receipts WHERE namespace=? AND id=?").get(identity.namespace, identity.id.toLowerCase()) as Row | null;
    if (!row) return;
    if (row.session !== identity.session || row.content !== identity.content || row.native_message_id !== identity.native_message_id) throw new Error("Broker message ID mapping changed");
    return row;
  }
  prepare(identity: Identity): Row {
    return this.db.transaction(() => {
      const existing = this.find(identity);
      if (existing) return existing;
      const size = this.db.query("SELECT count(*) AS rows,coalesce(sum(length(CAST(content AS BLOB))+coalesce(length(CAST(acceptance AS BLOB)),0)),0) AS bytes FROM receipts").get() as { rows: number; bytes: number };
      if (size.rows >= MAX_JOURNAL_ROWS || size.bytes + Buffer.byteLength(identity.content) > MAX_JOURNAL_BYTES) throw new JournalCapacityError("Receipt journal capacity reached; retained IDs cannot be evicted");
      this.db.query("INSERT INTO receipts(namespace,id,session,content,native_message_id,state) VALUES(?,?,?,?,?,'prepared')").run(identity.namespace, identity.id.toLowerCase(), identity.session, identity.content, identity.native_message_id);
      const row = this.find(identity);
      if (!row) throw new Error("Cannot persist message mapping");
      return row;
    }).immediate();
  }
  attempt(identity: Identity): boolean {
    return this.db.query("UPDATE receipts SET state='attempting' WHERE namespace=? AND id=? AND state='prepared'").run(identity.namespace, identity.id.toLowerCase()).changes === 1;
  }
  inserted(identity: Identity): void {
    this.db.query("UPDATE receipts SET state='inserted' WHERE namespace=? AND id=?").run(identity.namespace, identity.id.toLowerCase());
  }
  accepted(identity: AttemptIdentity, receipt: unknown): void {
    const native = validateAcceptance(identity, receipt);
    // Identity/content already live in the immutable row. Retain only bounded
    // native receipt facts and original dispatch provenance, not duplicate text
    // or arbitrary native metadata/attachments.
    const proof = JSON.stringify({ generation: identity.generation, attempt: identity.attempt, type: native.type, delivery: native.delivery, created: native.time.created });
    this.db.transaction(() => {
      this.find(identity);
      const size = this.db.query("SELECT coalesce(sum(length(CAST(content AS BLOB))+coalesce(length(CAST(acceptance AS BLOB)),0)),0) AS bytes FROM receipts").get() as { bytes: number };
      if (size.bytes + Buffer.byteLength(proof) > MAX_JOURNAL_BYTES) throw new JournalCapacityError("Receipt proof exceeds retained journal byte capacity");
      const changed = this.db.query("UPDATE receipts SET state='inserted',acceptance=? WHERE namespace=? AND id=? AND state='attempting'").run(proof, identity.namespace, identity.id.toLowerCase()).changes;
      if (changed !== 1) throw new Error("Native acceptance has no matching durable attempt fence");
    }).immediate();
  }
  hasAcceptance(identity: AttemptIdentity, readonly = false): boolean {
    const row = this.find(identity);
    if (row?.state !== "inserted" || !row.acceptance) return false;
    const proof = JSON.parse(row.acceptance);
    const uuid = /^[a-fA-F0-9]{8}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{4}-[a-fA-F0-9]{12}$/;
    if (!proof || proof.generation !== identity.generation || typeof proof.attempt !== "string" || !uuid.test(proof.attempt) || (!readonly && proof.attempt !== identity.attempt) || proof.type !== "user" || proof.delivery !== "steer" || typeof proof.created !== "number" || !Number.isFinite(proof.created)) return false;
    // A reconciliation request has a fresh response-fencing nonce, NOT the
    // original dispatch attempt. Original provenance was committed only by
    // accepted() after the guarded winning prompt; it is never rewritten here.
    return true;
  }
  close() { this.db.close(); }
}
