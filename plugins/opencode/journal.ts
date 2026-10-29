import { Database } from "bun:sqlite";
import { constants } from "node:fs";
import { lstat, open, realpath } from "node:fs/promises";
import { join } from "node:path";
import { createHash } from "node:crypto";

export type Identity = { namespace: string; id: string; session: string; content: string; native_message_id: string };
export class JournalCapacityError extends Error {}
export const MAX_JOURNAL_ROWS = 65536;
export const MAX_JOURNAL_BYTES = 64 * 1024 * 1024;
type Row = Identity & { state: "prepared" | "attempting" | "inserted" };
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
      if (version !== 0 && version !== 1) throw new Error("Unsupported receipt journal schema");
      if (version === 0) {
        if ((db.query("SELECT count(*) AS n FROM sqlite_master WHERE type='table'").get() as { n: number }).n !== 0) throw new Error("Unrecognized receipt journal");
        db.exec("CREATE TABLE receipts(namespace TEXT NOT NULL,id TEXT NOT NULL,session TEXT NOT NULL,content TEXT NOT NULL,native_message_id TEXT NOT NULL UNIQUE,state TEXT NOT NULL CHECK(state IN ('prepared','attempting','inserted')),PRIMARY KEY(namespace,id)) STRICT; PRAGMA user_version=1;");
      }
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
      const size = this.db.query("SELECT count(*) AS rows,coalesce(sum(length(CAST(content AS BLOB))),0) AS bytes FROM receipts").get() as { rows: number; bytes: number };
      if (size.rows >= MAX_JOURNAL_ROWS || size.bytes + Buffer.byteLength(identity.content) > MAX_JOURNAL_BYTES) throw new JournalCapacityError("Receipt journal capacity reached; retained IDs cannot be evicted");
      this.db.query("INSERT INTO receipts VALUES(?,?,?,?,?,'prepared')").run(identity.namespace, identity.id.toLowerCase(), identity.session, identity.content, identity.native_message_id);
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
  close() { this.db.close(); }
}
