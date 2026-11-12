# Roadmap

## 0.1 — Foundation

- [x] Establish Rust workspace and crate boundaries.
- [x] Document Wayland-only scope and security principles.
- [ ] Add CI quality gates and dependency policy.
- [ ] Validate the dependency stack and MSRV on CI.
- [x] Add a minimal native Wayland xdg-toplevel window bootstrap (placeholder SHM surface).

## 0.2 — Terminal core

- [ ] Integrate a mature VT engine behind an internal adapter.
- [x] Add PTY lifecycle management and bounded I/O.
- [x] Implement PTY winsize updates and kernel SIGWINCH regression tests.
- [x] Add Wayland surface-to-PTY resize conversion and validation.
- [x] Wire the PTY resize adapter into the native Wayland configure/commit event loop.
- [ ] Add VT and PTY regression fixtures.

## 0.3 — Rendering

- [x] Add safely bounded, resize-aware Wayland SHM placeholder buffers and compositor-release tracking.\n- [ ] Establish GPU renderer and explicit software fallback behavior.
- [ ] Add font shaping, glyph atlas, HiDPI, and frame scheduling.
- [ ] Benchmark large output and long-running sessions.

## 0.4 — Interface

- [ ] Build the custom Bubble Glass interface.
- [ ] Add tabs, splits, keyboard navigation, and responsive layouts.
- [ ] Add configuration UI and accessibility support.

## 0.5 — Hardening

- [ ] Fuzz VT/OSC/DCS parsing and image payload handling.
- [ ] Test GNOME, KDE Plasma, Sway, Hyprland, and BubbleWM.
- [ ] Add packaging, release provenance, and Debian integration.

Milestones are acceptance gates, not promises of feature completeness.
