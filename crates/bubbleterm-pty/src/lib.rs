//! PTY process lifecycle boundary.
//!
//! The implementation will explicitly own child processes, propagate resize
//! and signals, apply bounded I/O, and reap children during shutdown.

#![forbid(unsafe_code)]
