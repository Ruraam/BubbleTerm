# Security Policy

## Reporting

Please report suspected vulnerabilities privately to the repository maintainer rather than publishing exploit details in an issue. A dedicated security contact will be documented here when configured.

## Security principles

- Terminal output is untrusted input.
- OSC, DCS, image payloads, hyperlinks, and clipboard operations must be validated and bounded.
- Clipboard reads/writes and potentially sensitive integrations require explicit policy and user consent.
- Child processes must be tracked and reaped; closing a tab must not leave unintended process trees.
- Plugins, if introduced, must use a permissioned and resource-limited interface.
- Logs must not capture secrets or clipboard contents by default.

## Scope

The project targets Linux Wayland only. Security reports affecting the terminal parser, PTY management, renderer, configuration loading, or plugin boundary are in scope.
