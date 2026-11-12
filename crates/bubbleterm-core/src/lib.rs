//! Shared application-level types and contracts for BubbleTerm.

#![forbid(unsafe_code)]

/// Identifies a terminal session within the running application.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(u64);

impl SessionId {
    /// Creates an identifier from an application-assigned numeric value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::SessionId;

    #[test]
    fn session_id_round_trips() {
        let id = SessionId::new(42);
        assert_eq!(id.get(), 42);
    }
}
