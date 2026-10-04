//! The kinds of account an administrator can mark one as (`User.user_type` in the admin API,
//! `user_type` in `POST /register` and `hs register --user-type`): the two values Synapse
//! defines, so that an imported account keeps its meaning and an exported one reads the same.
//!
//! Recording the kind changes nothing about what the account may do. It is shown on the
//! account, carried on `hs-admin`'s `User`, and is the vocabulary a later reader (a user
//! directory that hides support accounts, statistics that count people only) would key on.

/// The kinds an account can be marked as, besides a person: Synapse's two values.
pub const USER_TYPES: &[&str] = &["bot", "support"];

/// Reads a `user_type` as given by an administrator: `None` or an empty string is a person
/// (`Ok(None)`); `bot` and `support` are themselves, trimmed and lower-cased; anything else is
/// refused with a sentence fit to show beside the field.
///
/// # Errors
/// The reason, naming the accepted values.
pub fn parse_user_type(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let kind = raw.trim().to_ascii_lowercase();
    if kind.is_empty() {
        return Ok(None);
    }
    if USER_TYPES.contains(&kind.as_str()) {
        Ok(Some(kind))
    } else {
        Err(format!(
            "{raw:?} is not a kind of account; use bot or support, or none for a person"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_is_none_and_the_two_kinds_are_normalised() {
        assert_eq!(parse_user_type(None), Ok(None));
        assert_eq!(parse_user_type(Some("")), Ok(None));
        assert_eq!(parse_user_type(Some("  ")), Ok(None));
        assert_eq!(parse_user_type(Some("bot")), Ok(Some("bot".to_string())));
        assert_eq!(
            parse_user_type(Some(" Support ")),
            Ok(Some("support".to_string()))
        );
    }

    #[test]
    fn anything_else_is_refused_by_name() {
        let err = parse_user_type(Some("admin")).unwrap_err();
        assert!(err.contains("\"admin\""), "{err}");
        assert!(err.contains("bot or support"), "{err}");
    }
}
