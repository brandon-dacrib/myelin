//! Turning what an administrator typed into the user ID of an account on *this* server.
//!
//! Shared by first-run setup ([`crate::setup`]) and the admin API's user creation
//! ([`crate::admin_directory`]), so that "what counts as a username" has one answer. `POST
//! /register` has its own path because the client-server spec dictates its errors.

use ruma::{OwnedUserId, ServerName, UserId};

/// `alice`, `@alice`, and `@alice:<this server>` all mean the same account; anything naming
/// another server does not belong here.
///
/// The localpart is lower-cased, for the same reason `POST /register` does it -- `Ops` and `ops`
/// must not be two accounts -- and is then held to the strict grammar the spec gives for *new*
/// user IDs (`a-z 0-9 . _ = - / +`). `UserId::parse_with_server_name` alone is not enough: it
/// also accepts the historical localparts the spec tolerates on existing accounts.
///
/// # Errors
/// A sentence fit to show beside the username field.
pub fn local_user_id(server_name: &ServerName, username: &str) -> Result<OwnedUserId, String> {
    let trimmed = username.trim();
    let unsigiled = trimmed.strip_prefix('@').unwrap_or(trimmed);
    let localpart = match unsigiled.split_once(':') {
        None => unsigiled,
        Some((localpart, server)) if server == server_name.as_str() => localpart,
        Some((_, server)) => return Err(format!("this server is {server_name}, not {server}")),
    };
    if localpart.is_empty() {
        return Err("choose a username".to_owned());
    }
    let localpart = localpart.to_ascii_lowercase();
    let unusable = || {
        format!(
            "\"{localpart}\" cannot be a username: use lowercase letters, digits, and any of . _ = - /"
        )
    };
    if !localpart
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._=-/+".contains(&b))
    {
        return Err(unusable());
    }
    UserId::parse_with_server_name(localpart.as_str(), server_name).map_err(|_| unusable())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(username: &str) -> Result<String, String> {
        let server = <&ServerName>::try_from("example.org").unwrap();
        local_user_id(server, username).map(|u| u.to_string())
    }

    #[test]
    fn every_spelling_of_a_local_account_is_the_same_account() {
        for spelling in ["ops", "@ops", "Ops", " ops ", "@OPS:example.org"] {
            assert_eq!(
                parse(spelling).as_deref(),
                Ok("@ops:example.org"),
                "{spelling:?}"
            );
        }
        assert_eq!(
            parse("a.b_c=d-e/f+g1").as_deref(),
            Ok("@a.b_c=d-e/f+g1:example.org")
        );
    }

    #[test]
    fn another_servers_user_is_refused_by_name() {
        let err = parse("@ops:elsewhere.org").unwrap_err();
        assert!(
            err.contains("example.org") && err.contains("elsewhere.org"),
            "{err}"
        );
    }

    #[test]
    fn what_cannot_be_a_new_localpart_is_refused() {
        for bad in [
            "",
            "@",
            "not a username",
            "üser",
            "semi;colon",
            "@:example.org",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
