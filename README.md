# BubbleTerm

A modern, production-oriented terminal emulator for Linux Wayland, written in Rust.

**BubbleTerm is the reference terminal for BubbleOS.** It is designed around BubbleOS's Wayland-native and sandbox-first philosophy while remaining a standalone terminal that runs on mainstream Wayland compositors such as Hyprland and Sway.

X11 is intentionally out of scope.

## Project status

**Early development — foundation phase.** APIs, crate boundaries, and implementation details may evolve as the project is validated.

## Core Pillars

### Wayland-first

BubbleTerm is a native Wayland client. There is no X11 backend: the architecture targets modern Linux compositors directly.

### Sandbox / Bubblewrap

**Sandboxing is a core part of BubbleTerm's identity.**

Terminal sessions can run either directly on the host or inside a BubbleOS-oriented isolated environment powered by Bubblewrap (`bwrap`). The PTY layer exposes this through an explicit execution policy:

- **Host** — run the selected shell normally as the current user.
- **Bubble** — run the selected shell through `bwrap` with an explicit rootfs and read-only or read-write policy.

The sandbox is deliberately explicit: requested isolation must not silently fall back to a host shell. Namespace and filesystem boundaries are part of the terminal's execution model, making the **Bubble** in BubbleTerm more than a visual identity.

### Production-grade terminal core

BubbleTerm targets robust terminal emulation, careful PTY lifecycle management, synchronized terminal resizing, secure input handling, clipboard support, and a well-tested VT implementation.

### Modern rendering and interface

GPU-accelerated rendering is paired with a controlled software fallback and a responsive custom interface built around the **Bubble Glass** visual identity.

### Bundled Unicode glyphs

The software renderer keeps antialiased Noto Sans Mono glyphs for supported Latin ranges and uses an embedded GNU Unifont bitmap fallback for broader Unicode coverage, including Greek, Cyrillic, and CJK. Glyph pages are decompressed lazily and cached; wide terminal cells are rendered across their two-column span.

This bitmap fallback provides glyph lookup, not a complete text-shaping engine. Common Latin combining diacritics are normalized and overlaid, but full mark positioning, complex-script shaping, bidirectional layout, and emoji sequences still require dedicated work. Do not interpret broad glyph coverage as full Unicode shaping support.

Third-party font notices: `noto-sans-mono-bitmap` is distributed under its crate license; its bundled Noto font data and GNU Unifont data retain their respective upstream font licenses. GNU Unifont 14.0.01 is distributed under SIL Open Font License 1.1 and GPLv2+ with the GNU Font Embedding Exception. Preserve upstream notices when redistributing binaries. See the upstream [Unifont license information](https://unifoundry.com/unifont/index.html) and [`unifont-bitmap` crate documentation](https://docs.rs/unifont-bitmap/1.0.0+unifont-14.0.01/unifont_bitmap/).

### BubbleOS-native, compositor-independent

BubbleTerm is the official reference terminal for BubbleOS, but it is not locked to BubbleOS or BubbleWM. The same Wayland-native application is intended to work on compatible compositors including **Hyprland** and **Sway**.

## Deployment goals

- Native Wayland deployment with no X11 backend.
- **BubbleOS integration as the reference terminal.**
- **Debian as a primary deployment target**, with reproducible packaging and distribution workflows.
- Compatibility with a **Debian / Limine-based BubbleOS deployment stack**.
- Reproducible builds and strict CI validation.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md), [ARCHITECTURE.md](docs/ARCHITECTURE.md), and [ROADMAP.md](docs/ROADMAP.md).

## License

License selection is pending. Do not assume this project is licensed for redistribution until a license file is added.
