# Architecture

## Current foundation

The workspace separates application orchestration from terminal semantics, process management, Wayland integration, rendering, UI, configuration, and security policy.

```text
bubbleterm-app
  ├── bubbleterm-core
  ├── bubbleterm-ui
  ├── bubbleterm-wayland
  ├── bubbleterm-renderer
  ├── bubbleterm-pty
  ├── bubbleterm-vt
  ├── bubbleterm-config
  └── bubbleterm-security
```

The initial crates are intentionally small. Dependencies will be introduced behind crate-local interfaces and validated by CI before being treated as architectural commitments.

## Boundaries

- **app**: executable, startup, shutdown, and top-level wiring.
- **core**: shared domain types and application-level contracts.
- **vt**: terminal grid, parser integration, modes, and scrollback.
- **pty**: child process creation, PTY I/O, resize, signals, reaping, and execution-mode selection.
- **wayland**: native Wayland client lifecycle and compositor protocols.
- **renderer**: GPU resources, glyph atlas, frame scheduling, and fallback policy.
- **ui**: custom interface, tabs, splits, command palette, and accessibility.
- **config**: schema, validation, defaults, and migration.
- **security**: permission decisions, limits, and safe handling policies.

## PTY execution modes

Every terminal session is created from a backend-independent `LaunchSpec` containing a `ShellKind`:

1. **Host** — launch the selected shell as the current user.
2. **Bubble** — launch the selected shell through Bubblewrap (`bwrap`) with the configured rootfs mounted at `/`, read-only by default.

`bubbleterm-pty` uses `portable-pty` for PTY allocation, process spawning, reader/writer handles, child lifecycle, and winsize updates. On Unix, its spawn path creates a new session and assigns the PTY slave as the child's controlling terminal with `TIOCSCTTY`. Bubble mode intentionally does not pass Bubblewrap's `--new-session`: creating another session after PTY setup would detach the controlling terminal and break normal job control and resize signal delivery.

The Wayland configure handler must translate the committed surface dimensions and current cell metrics into a `PtySize`, then call `PtySession::resize` on the corresponding PTY master. The kernel updates the slave's winsize and delivers `SIGWINCH` to the foreground process group; BubbleTerm should not send a second synthetic signal. The PTY resize API and kernel-winsize tests are present; wiring this call into a real Wayland event loop remains pending because the current Wayland crate is still only a foundation stub.

The VT engine and UI consume the same PTY interface in both modes. They must not contain mode-specific process logic. Arguments are passed directly to process APIs; they are never interpolated into a shell command. If Bubble mode is requested and Bubblewrap or the required namespaces/mounts are unavailable, session creation must fail rather than silently falling back to a host shell.

The sandbox is a process/filesystem namespace boundary, not a complete virtual machine. It does not by itself provide a kernel security boundary against host-kernel vulnerabilities.

The launch policy is deliberately separate from the PTY transport. This keeps future backends (for example, a remote or container-backed session) possible without changing VT semantics.

## Non-goals

- X11 support.
- Requiring BubbleOS or BubbleWM.
- Automatically editing shell startup files.
- Treating terminal escape sequences as trusted commands.
