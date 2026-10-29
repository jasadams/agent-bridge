# Updater lock lifetime regression

The updater's automatic checker intentionally skips a check when another checker
holds `update.lock`. Its lock must therefore end when the owning operation
returns, including throttled, error and unwind paths.

Unix `flock` locks belong to an open file description shared by duplicated or
fork-inherited descriptors. Closing the parent's `File` alone does not release
that lock while a child retains the description. `CLOEXEC` closes a descriptor at
exec; it does not prevent inheritance between fork and exec. A concurrent process
spawn can consequently extend a completed check's lock and make a due automatic
check take the nonblocking competing-check path.

The controlled regression calls the real `check_with`: an automatic fetch, a
throttled check, then a forced fetch that forks a child while the lock is held.
The child reports readiness and retains its descriptors behind an explicit gate.
The next due automatic check must perform the third fetch before that gate opens.
Against unchanged production code from `a00d963ab1782192b53c290d3779ac7f0906a6f5`,
this failed deterministically with two fetches instead of three. The focused
before-fix inventory was nine passes and this one failure, exit 101.

This demonstrates a responsible production interleaving matching the intermittent
full-suite assertion. Historical CI did not trace its fork timing, so this is not
a claim that every previous failure's scheduling was captured.

The repair wraps only successfully acquired locks in a guard that explicitly
unlocks before closing, across checks, preference writes and installed-version
reconciliation. A failed nonblocking acquisition never creates the guard. Cache
timestamps, failure persistence, forced-check bypass and competing-check behavior
remain unchanged. Guard destruction cannot return an error; closing remains the
fallback if explicit unlock fails.

Run the isolated updater regressions with the repository's pinned toolchain:

```sh
cargo test --locked -p agent-bridge --lib update::tests --no-fail-fast
```

The fork fixtures use only async-signal-safe raw system calls in the child and
`_exit`, never inherited Rust destructors. A bounded poll and parent release/wait
cleanup prevent stranded children. Success requires the child to exit through the
explicit release gate, so a timeout cannot turn a delayed check into a false pass.
Additional cases cover fetch failure and panic while preserving the attempt
throttle. State and child processes are disposable; no public network, owner
configuration or running service is needed for these lock regressions.
