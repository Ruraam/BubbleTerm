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
- **wayland**: native Wayland client lifecycle, compositor protocols, keyboard input, and surface-to-PTY resize adaptation.
- **renderer**: GPU resources, glyph atlas, frame scheduling, and fallback policy.
- **ui**: custom interface, tabs, splits, command palette, and accessibility.
- **config**: schema, validation, defaults, and migration.
- **security**: permission decisions, limits, and safe handling policies.

## PTY execution modes

Every terminal session is created from a backend-independent `LaunchSpec` containing a `ShellKind`:

1. **Host** — launch the selected shell as the current user.
2. **Bubble** — launch the selected shell through Bubblewrap (`bwrap`) with the configured rootfs mounted at `/`, read-only by default.

`bubbleterm-pty` uses `portable-pty` for PTY allocation, process spawning, reader/writer handles, child lifecycle, and winsize updates. On Unix, its spawn path creates a new session and assigns the PTY slave as the child's controlling terminal with `TIOCSCTTY`. Bubble mode intentionally does not pass Bubblewrap's `--new-session`: creating another session after PTY setup would detach the controlling terminal and break normal job control and resize signal delivery.

The Wayland crate provides a native `wayland-client` lifecycle for an `xdg-toplevel`: it connects through `WAYLAND_DISPLAY`, binds `wl_compositor`, `wl_shm`, `wl_seat`, and `xdg_wm_base`, answers compositor pings, acknowledges `xdg_surface.configure`, and handles the toplevel close event. It currently maps a neutral shared-memory placeholder surface; this is a window bootstrap, not terminal rendering.

After acknowledging each `xdg_surface.configure`, the window handler calls `resize_pty_for_surface` for its owned PTY session. The adapter uses floor division to expose only complete terminal cells, preserves the last known size when the compositor sends state-only or zero-sized configure data, validates cell metrics, and applies the resulting `PtySize` through `PtySession::resize`. The kernel updates the slave's winsize and delivers `SIGWINCH` to the foreground process group; BubbleTerm must not send a second synthetic signal.

The window allocates a fresh SHM buffer for each committed size change, applies explicit dimension and 64 MiB allocation limits, and tracks `wl_buffer.release` so replaced buffers are retained until the compositor has finished using them. The content remains a neutral placeholder, not terminal output. The current bootstrap assumes scale factor 1; output-scale/fractional-scale negotiation, VT rendering, pointer input, and application-level session orchestration remain outstanding.

### Keyboard input

The window listens for `wl_seat` keyboard capability changes and obtains a `wl_keyboard` when available. `bubbleterm-wayland::keyboard::XkbKeyboard` compiles the keymap file descriptor sent by the compositor, applies the compositor's modifier/layout snapshots, and translates evdev keycodes using the XKB state. Printable UTF-8 text and common terminal key sequences (Enter, Backspace, Tab, Escape, navigation and editing keys) are written to the PTY master.

The active layout is never hardcoded: AZERTY, QWERTY, alternate groups, and modifier behavior follow the compositor-provided keymap. Key repeat policy/timing, richer terminal modes (application cursor/keypad), clipboard, pointer input, and end-to-end compositor testing are still pending. The current implementation is an input foundation only; it does not yet display the PTY output.

The VT engine and UI consume the same PTY interface in both modes. They must not contain mode-specific process logic. Arguments are passed directly to process APIs; they are never interpolated into a shell command. If Bubble mode is requested and Bubblewrap or the required namespaces/mounts are unavailable, session creation must fail rather than silently falling back to a host shell.

The sandbox is a process/filesystem namespace boundary, not a complete virtual machine. It does not by itself provide a kernel security boundary against host-kernel vulnerabilities.

The launch policy is deliberately separate from the PTY transport. This keeps future backends (for example, a remote or container-backed session) possible without changing VT semantics.

## Non-goals

- X11 support.
- Requiring BubbleOS or BubbleWM.
- Automatically editing shell startup files.
- Treating terminal escape sequences as trusted commands.
