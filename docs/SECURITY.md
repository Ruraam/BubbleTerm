# Security Design

## Untrusted input

All bytes received from a PTY are untrusted. Parser state, string lengths, image dimensions, hyperlink targets, and clipboard payloads must have explicit limits.

## Process lifecycle

The PTY layer must define ownership, shutdown, signal forwarding, resize propagation, and child reaping. No process should be detached accidentally when a tab or window closes.

## Sensitive operations

OSC 52 clipboard access, external URL opening, file/image protocols, and future integrations must be controlled by explicit policy. Dangerous or ambiguous actions must not be silently performed.

## Resource limits

Queues, scrollback, parser buffers, image storage, and plugin execution must be bounded. Backpressure must propagate instead of allowing unbounded memory growth.

## Wayland

Use compositor protocols only when available and negotiate optional extensions safely. Do not assume that blur, fractional scaling, decorations, or clipboard extensions are universally supported.
