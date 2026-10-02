# 0021. The user directory should find the remote members of shared and public rooms

Status: **proposed**, 2026-10-02 (branch `agent/user-sytest`). Author: track 05 (sync). Owner of
the change: track 07 (auth and identity), which serves `POST /user_directory/search`
(`crates/hs-auth/src/routes/user_directory.rs`); track 05 provides the answer. Affects:
`hs_auth::state::UserDirectoryVisibility`, `crates/hs-auth/src/routes/user_directory.rs`,
`crates/hs-user/src/hub.rs`.

## The problem

`POST /user_directory/search` ranks the accounts of **this** server (`AuthStore::list_users`)
and keeps those the room layer says the requester may see (`UserDirectoryVisibility::visible_to`,
answered by `hs_user::hub::SessionHub` from its directory index). A member of a shared or public
room whose account is on another server is in that answer and never in the ranking: the route
has no name or avatar for them, because it only knows local profiles.

Synapse's directory is built from the rooms, not the account table: a remote member's display
name and avatar come from their `m.room.member` event, and they are searchable like anybody who
shares a room. Sytest's "User in remote room doesn't appear in user directory after server left
room" has the remote server's user search for the creator (a remote user to that server) while
they share a public room, and expects to find them; that is the one user-directory Sytest left
failing after this branch (status 05, session 14).

## The change

`UserDirectoryVisibility::visible_to` returns, for every user the requester may find, what the
directory needs to show them:

```rust
pub struct DirectoryEntry {
    pub user_id: OwnedUserId,
    /// From the member event the directory index kept, for a remote user; `None` for a local
    /// one, whose current profile the auth store has.
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}
async fn visible_to(&self, requester: &UserId) -> Result<Vec<DirectoryEntry>, String>;
```

The route ranks the union: local accounts as now (profile from the auth store), and every
entry whose user is not a local account with the name and avatar the entry carries. Ranking,
limit and the deactivated-account rule are unchanged.

For the hub to carry names, its directory index (`hs_user.room_members`, `(room, user)`) gains
the member's `displayname` and `avatar_url` from the membership event that put them there --
written where the index is kept current (`SessionHub::index_members_for_directory`, one more
field per row) and rebuilt the same way a missing room is indexed today.

## Migration

Additive: rows without a name are a user the directory shows by id alone until their next
membership change re-indexes them. Nothing in the client-server API changes. The route's
`user_directory_search_all_users` mode is unaffected (it ranks local accounts only, as the
option says).

## Why not now

It touches the trait `hs-auth` owns and the route's ranking; the branch this is proposed on
already changed that route's one line about the requester themself (decision in status 05,
session 14), and a second cross-crate change there is better reviewed by its owner.
