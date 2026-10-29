-- Remove later local OpenCode recovery state when reconstructing a genuine v10 fixture.
DROP TRIGGER IF EXISTS opencode_enqueue_bound;
DROP TRIGGER IF EXISTS opencode_retry_bound;
DROP TRIGGER IF EXISTS opencode_enqueue_binding;
DROP TABLE IF EXISTS opencode_deliveries;
DROP TABLE IF EXISTS opencode_leases;
-- Restore the title tables when reconstructing an older numbered fixture from
-- a current database. This is test setup, not a supported database downgrade.
CREATE TABLE native_claude(session TEXT PRIMARY KEY,display_name TEXT);
CREATE TABLE native_codex(session TEXT PRIMARY KEY,display_name TEXT);
INSERT INTO native_claude SELECT session,display_name FROM session_metadata WHERE provider='claude';
INSERT INTO native_codex SELECT session,display_name FROM session_metadata WHERE provider='codex';
DROP TABLE session_metadata;
