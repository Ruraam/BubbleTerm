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

Every terminal session is created from a backend-independent `LaunchSpec` containing a `ShellKind`. The PTY backend supports two execution modes:

1. **Host** — launch the selected shell as the current user.
2. **Sandbox** — launch the selected shell through Bubblewrap (`bwrap`) using an explicit `SandboxConfig`.

The VT engine and UI consume the same PTY interface in both modes. They must not contain mode-specific process logic. Profiles may select the mode, and each session records the effective policy for diagnostics.

Sandbox policy is deny-by-default:
- Network isolation is the default; host networking must be an explicit opt-in.
- Filesystem access is granted through explicit read-only or read-write mounts.
- PID, IPC, UTS, and temporary-directory isolation are independently represented.
- The backend must pass arguments directly to process APIs, never interpolate them into a shell command.
- If sandbox mode is requested and Bubblewrap, namespaces, or required mounts are unavailable, session creation fails. There is no silent fallback to a host shell.
- The sandbox is a process/filesystem namespace boundary, not a complete virtual machine. It does not by itself provide a kernel security boundary against host-kernel vulnerabilities.

The launch policy is deliberately separate from the PTY transport. This keeps future backends (for example, a remote or container-backed session) possible without changing VT semantics.

## Non-goals

- X11 support.
- Requiring BubbleOS or BubbleWM.
- Automatically editing shell startup files.
- Treating terminal escape sequences as trusted commands.
