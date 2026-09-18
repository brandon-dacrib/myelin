//! Pagination tokens over a room's room-local timeline positions.
//!
//! A room's timeline position (`room_pos`, `PLAN.md` section 6.2) is a room-local, monotonically
//! increasing `i64` assigned by the room's own actor at persist time: positive and increasing for
//! events the room actor originates or accepts live, negative and decreasing for events inserted
//! by backfill (older than anything the room actor had when backfill started). A
//! [`PaginationToken`] is just an opaque wrapper around one such position plus a direction, which
//! is all `/messages`, `/context` and `/relations` need to say "continue from here".

use std::fmt;
use std::str::FromStr;

/// Which way a `/messages`-style pagination token continues from its position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Walk towards increasing `room_pos` (newer events).
    Forward,
    /// Walk towards decreasing `room_pos` (older events) -- the common case for `/messages?dir=b`.
    Backward,
}

impl Direction {
    /// Parses the spec's `dir` query parameter (`"f"` or `"b"`).
    #[must_use]
    pub fn from_query(s: &str) -> Option<Self> {
        match s {
            "f" => Some(Self::Forward),
            "b" => Some(Self::Backward),
            _ => None,
        }
    }
}

/// An opaque pagination token: a room-local timeline position and the direction it continues in.
///
/// Encodes as `<f|b><room_pos>` (for example `b42`, `f-17`), which is not part of the spec
/// contract (tokens are opaque to clients) but is stable and easy to reason about in tests and
/// logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaginationToken {
    /// The room-local position this token was issued at or continues from.
    pub room_pos: i64,
    /// Which way this token continues.
    pub direction: Direction,
}

impl PaginationToken {
    /// A token at `room_pos`, continuing in `direction`.
    #[must_use]
    pub fn new(room_pos: i64, direction: Direction) -> Self {
        Self { room_pos, direction }
    }
}

impl fmt::Display for PaginationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dir = match self.direction {
            Direction::Forward => 'f',
            Direction::Backward => 'b',
        };
        write!(f, "{dir}{}", self.room_pos)
    }
}

impl FromStr for PaginationToken {
    type Err = crate::error::RoomError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (dir_char, rest) = s
            .split_at_checked(1)
            .ok_or(crate::error::RoomError::InvalidPaginationToken)?;
        let direction = match dir_char {
            "f" => Direction::Forward,
            "b" => Direction::Backward,
            _ => return Err(crate::error::RoomError::InvalidPaginationToken),
        };
        let room_pos = rest
            .parse::<i64>()
            .map_err(|_| crate::error::RoomError::InvalidPaginationToken)?;
        Ok(Self { room_pos, direction })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_display_and_from_str() {
        for token in [
            PaginationToken::new(42, Direction::Backward),
            PaginationToken::new(-17, Direction::Forward),
            PaginationToken::new(0, Direction::Backward),
        ] {
            let s = token.to_string();
            let parsed: PaginationToken = s.parse().unwrap();
            assert_eq!(parsed, token);
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!("".parse::<PaginationToken>().is_err());
        assert!("x42".parse::<PaginationToken>().is_err());
        assert!("bnotanumber".parse::<PaginationToken>().is_err());
    }

    #[test]
    fn direction_from_query_matches_spec_values() {
        assert_eq!(Direction::from_query("f"), Some(Direction::Forward));
        assert_eq!(Direction::from_query("b"), Some(Direction::Backward));
        assert_eq!(Direction::from_query("x"), None);
    }
}
