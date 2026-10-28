# Coding standards

- Follow existing Rust module boundaries: CLI in `src/main.rs`, persistence and
  migrations in their dedicated modules, and peer transport/trust under
  `src/network/`. Extend existing abstractions before adding parallel paths.
- Preserve durable delivery, revision checks, explicit peer/session grants, and
  pairing/trust boundaries. Read the relevant protocol and security docs before
  changing wire formats, access checks, or persistent state.
- Handle fallible operations explicitly and attach useful error context. Do not
  hide failures with successful defaults, suppress lint warnings to bypass a
  defect, or add production shortcuts for tests.
- Keep changes focused. Avoid unrelated refactors, dependency updates, vendor
  edits, or baseline-wide formatting. Document compatibility changes and update
  feature documentation when user-visible behavior changes.
- Add regression coverage for changed behavior, including meaningful failure and
  boundary cases. Prefer real local implementations and isolated temporary state;
  do not depend on a user's service, credentials, session data, or public relay.
- Never log secrets, pairing material, or private message contents unnecessarily.
  Treat incoming peer messages and stored session metadata as untrusted input.
- Use the validation and independent review process in `.claude/build-test.md`.
  Review records need a written verdict, not cryptographic signing. Standards
  maintenance is separate work; do not mine review logs on every ticket.
