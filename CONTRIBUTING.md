# Contributing

Matari accepts focused changes that preserve its native protocol boundaries:

- drag lifecycle state advances only from native events;
- no startup or finish delay, timer, pointer polling, or watchdog may infer a
  protocol outcome;
- a toolkit keeps ownership of its existing native connection and event queue;
- every editor owns at most one controller;
- file validation and native protocol work stay off realtime audio threads;
- every unsafe operation documents the invariant that makes it sound.

Before submitting a change, run:

```sh
cargo fmt --check
cargo check --locked --all-features
cargo clippy --locked --lib --all-features -- \
  -D warnings -D clippy::undocumented_unsafe_blocks -D unsafe_op_in_unsafe_fn
RUSTDOCFLAGS="-D warnings -D missing-docs" cargo doc --locked --no-deps
cargo publish --dry-run --locked
```

Native compatibility claims also need dated evidence from the named operating
system, host, plug-in format, presentation, source and target backends, payload,
and terminal protocol outcome.

## Toolkit hosts

`toolkit::X11Host` exists so an integration does not re-derive pointer
authority, session bookkeeping, and drop-router lifetime by hand. Prefer
extending a host over documenting another manual wiring: a step a toolkit has
to remember is a step some toolkit will omit, and an omitted drop router fails
silently rather than loudly.
