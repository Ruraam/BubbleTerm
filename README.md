# BubbleTerm

A modern, production-oriented terminal emulator for Linux Wayland, written in Rust.

BubbleTerm is designed to run on mainstream Wayland compositors independently of BubbleOS and BubbleWM. X11 is intentionally out of scope.

## Project status

**Early development — foundation phase.** APIs, crate boundaries, and implementation details may evolve as the project is validated.

## Goals

- Native Wayland client; no X11 backend.
- Robust terminal emulation with a well-tested VT implementation.
- GPU-accelerated rendering with a controlled software fallback.
- Responsive custom interface with the **Bubble Glass** visual identity.
- Strong process lifecycle, input, clipboard, and parser security.
- Reproducible builds, strict CI, and Debian packaging.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md), [ARCHITECTURE.md](docs/ARCHITECTURE.md), and [ROADMAP.md](docs/ROADMAP.md).

## License

License selection is pending. Do not assume this project is licensed for redistribution until a license file is added.
