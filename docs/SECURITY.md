# Security Design

## Untrusted input

All bytes received from a PTY are untrusted. Parser state, string lengths, image dimensions, hyperlink targets, and clipboard payloads must have explicit limits.

## Process lifecycle

The PTY layer owns the complete child-process lifecycle: spawn, I/O, resize, signal forwarding, shutdown, and reaping. Closing a tab must not leave detached descendants behind. Host and sandbox sessions use the same lifecycle contract.

## Host and Bubblewrap execution

Execution mode is an explicit per-session policy:
- Host mode runs the selected shell as the current user.
- Sandbox mode uses Bubblewrap and an explicit allowlist of filesystem mounts and capabilities.

Sandbox mode must fail closed. Never retry a failed sandbox launch as a host launch. Never construct a command string or invoke an intermediate shell to pass arguments to Bubblewrap. Use OS argument APIs so spaces and metacharacters remain data.

The default sandbox policy isolates networking and creates private PID, IPC, UTS, and temporary namespaces. Host networking and writable filesystem mounts require explicit configuration. The backend must validate absolute mount paths and reject malformed policies before spawning.

Bubblewrap relies on Linux namespaces and kernel security mechanisms. It reduces exposure but is not a VM and cannot protect against every kernel vulnerability. The UI must communicate the active mode clearly so users can distinguish host and sandbox sessions.

## Sensitive operations

OSC 52 clipboard access, external URL opening, file/image protocols, and future integrations must be controlled by explicit policy. Dangerous or ambiguous actions must not be silently performed. A sandboxed process must not gain clipboard, portal, device, or host filesystem access merely because the terminal UI has that access.

## Resource limits

Queues, scrollback, parser buffers, image storage, and plugin execution must be bounded. Backpressure must propagate instead of allowing unbounded memory growth. PTY output must remain untrusted in both execution modes.

## Wayland

Use compositor protocols only when available and negotiate optional extensions safely. Do not assume that blur, fractional scaling, decorations, or clipboard extensions are universally supported.
