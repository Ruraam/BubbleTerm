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
- **pty**: child process creation, PTY I/O, resize, signals, and reaping.
- **wayland**: native Wayland client lifecycle and compositor protocols.
- **renderer**: GPU resources, glyph atlas, frame scheduling, and fallback policy.
- **ui**: custom interface, tabs, splits, command palette, and accessibility.
- **config**: schema, validation, defaults, and migration.
- **security**: permission decisions, limits, and safe handling policies.

## Non-goals

- X11 support.
- Requiring BubbleOS or BubbleWM.
- Automatically editing shell startup files.
- Treating terminal escape sequences as trusted commands.
