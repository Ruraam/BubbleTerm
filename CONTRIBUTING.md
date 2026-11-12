# Contributing to BubbleTerm

## Scope

BubbleTerm is a native Linux Wayland terminal emulator written in Rust. X11 support is explicitly out of scope. The application must remain usable on Wayland compositors independently of BubbleOS and BubbleWM.

## Development requirements

- Rust stable, with `rustfmt` and `clippy`.
- A working Wayland session for compositor integration tests.
- Vulkan-capable graphics hardware is recommended; renderer fallbacks must be tested separately.

## Quality gates

Before submitting changes, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Changes to terminal parsing, PTY lifecycle, input handling, clipboard access, and image protocols require focused regression tests.

## Engineering rules

- Keep crate boundaries explicit and avoid cyclic dependencies.
- Do not execute shell commands through a shell when a direct process invocation is sufficient.
- Never silently modify user shell startup files.
- Treat terminal output, escape sequences, OSC payloads, and plugin data as untrusted input.
- Bound allocations and queues fed by child processes.
- Do not add X11 dependencies or fallback paths.
- Document protocol extensions and compositor-specific behavior.

## Pull requests

Describe the user-visible behavior, security implications, test coverage, and any compositor or hardware limitations. Do not merge without maintainer approval.
