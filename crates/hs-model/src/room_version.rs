//! The room-version capability table: room versions 1 to 12 plus the unstable identifiers
//! Synapse 1.161.0 ships (`docs/synapse-inventory.md`, "Room versions known to Synapse").
//!
//! This is the week-2 seam (`docs/workstreams/README.md`): every track that parses, authorizes,
//! hashes or redacts an event does it by asking this table what the room version requires, rather
//! than special-casing version numbers at call sites.
//!
//! The flag set is re-expressed from `ruma_common::room_version_rules`
//! (`refs/ruma/crates/ruma-common/src/room_version_rules.rs`, MIT license, Ruma project) with
//! attribution, flattened from five nested structs into one [`RoomVersionRules`] (plus
//! [`RedactionRules`], which stays separate because [`crate::redaction`] threads it through on its
//! own) and extended with the room versions Synapse advertises that Ruma does not model
//! (`org.matrix.hydra.11` and friends). Field-for-field agreement with Ruma's table for room
//! versions 1 to 12 is asserted in this module's tests.

use ruma::RoomVersionId;

/// Whether a room version has a stable specification or is still experimental.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomVersionDisposition {
    /// Specified in a released version of the Matrix specification.
    Stable,
    /// An MSC-gated identifier: behavior may still change.
    Unstable,
}

/// The format of event IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventIdFormat {
    /// `$id:server`, room version 1.
    V1Opaque,
    /// `$hash` using standard base64, room version 3.
    V2StandardBase64,
    /// `$hash` using URL-safe base64, room version 4 onward.
    V3UrlSafeBase64,
}

/// The format of room IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomIdFormat {
    /// `!id:server`, room versions 1 to 11.
    V1Opaque,
    /// `!hash` using the reference hash of the room's `m.room.create` event, room version 12
    /// (MSC4291, "hash-based room IDs").
    V2HashBased,
}

/// The format of the `auth_events` and `prev_events` arrays in a PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventsReferenceFormat {
    /// `[["$id:server", {"sha256": "hash"}]]`, room versions 1 and 2.
    V1WithHash,
    /// `["$hash"]`, room version 3 onward.
    V2IdOnly,
}

/// Which state resolution algorithm a room version uses, and its tweaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateResolutionVersion {
    /// State resolution v1, room version 1 only.
    V1,
    /// State resolution v2 (room version 2 to 11) or v2.1 (room version 12 onward, MSC4297).
    V2 {
        /// Begin the iterative auth checks with an empty state map rather than the unconflicted
        /// state, and consider the conflicted-state subgraph when building full conflicted
        /// state. Both flip together, from room version 12 (v2.1).
        v2_1: bool,
    },
}

/// The tweaks in the redaction algorithm for a room version. See [`crate::redaction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedactionRules {
    /// Keep `content.aliases` on `m.room.aliases`. Dropped from room version 6.
    pub keep_room_aliases_aliases: bool,
    /// Keep `content.allow` on `m.room.join_rules`. Added in room version 8.
    pub keep_room_join_rules_allow: bool,
    /// Keep `content.join_authorised_via_users_server` on `m.room.member`. Added in room
    /// version 9.
    pub keep_room_member_join_authorised_via_users_server: bool,
    /// Keep the top-level `origin`, `membership` and `prev_state` fields on every event. Dropped
    /// from room version 11.
    pub keep_origin_membership_prev_state: bool,
    /// Keep the entire `content` of `m.room.create` (instead of just `content.creator`). Added
    /// in room version 11.
    pub keep_room_create_content: bool,
    /// Keep `content.redacts` on `m.room.redaction`. Added in room version 11.
    pub keep_room_redaction_redacts: bool,
    /// Keep `content.invite` on `m.room.power_levels`. Added in room version 11.
    pub keep_room_power_levels_invite: bool,
    /// Keep `content.third_party_invite.signed` on `m.room.member`. Added in room version 11.
    pub keep_room_member_third_party_invite_signed: bool,
    /// Use `content.redacts` (rather than the top-level `redacts`) to find the event an
    /// `m.room.redaction` redacts. Added in room version 11.
    pub content_field_redacts: bool,
}

impl RedactionRules {
    /// Room version 1.
    pub const V1: Self = Self {
        keep_room_aliases_aliases: true,
        keep_room_join_rules_allow: false,
        keep_room_member_join_authorised_via_users_server: false,
        keep_origin_membership_prev_state: true,
        keep_room_create_content: false,
        keep_room_redaction_redacts: false,
        keep_room_power_levels_invite: false,
        keep_room_member_third_party_invite_signed: false,
        content_field_redacts: false,
    };
    /// Room version 6.
    pub const V6: Self = Self {
        keep_room_aliases_aliases: false,
        ..Self::V1
    };
    /// Room version 8.
    pub const V8: Self = Self {
        keep_room_join_rules_allow: true,
        ..Self::V6
    };
    /// Room version 9.
    pub const V9: Self = Self {
        keep_room_member_join_authorised_via_users_server: true,
        ..Self::V8
    };
    /// Room version 11.
    pub const V11: Self = Self {
        keep_origin_membership_prev_state: false,
        keep_room_create_content: true,
        keep_room_redaction_redacts: true,
        keep_room_power_levels_invite: true,
        keep_room_member_third_party_invite_signed: true,
        content_field_redacts: true,
        ..Self::V9
    };
}

/// The full per-version capability table entry.
///
/// Grouped loosely by concern (event/room ID format, state resolution, signatures, auth rules,
/// redaction) but kept as one flat struct: callers usually need one or two flags and a flat
/// struct keeps `RoomVersionRules::V6 { .. }`-style construction readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomVersionRules {
    /// Stable or MSC-gated.
    pub disposition: RoomVersionDisposition,

    // --- event / room ID and reference format ---
    /// Event ID format.
    pub event_id_format: EventIdFormat,
    /// Room ID format.
    pub room_id_format: RoomIdFormat,
    /// `auth_events` / `prev_events` array format.
    pub events_reference_format: EventsReferenceFormat,
    /// Whether the PDU must carry an explicit `event_id` field. Dropped from room version 3,
    /// where the event ID is derived from the reference hash instead.
    pub event_format_requires_event_id: bool,
    /// Whether `m.room.create` must carry a `room_id` field. Dropped from room version 12
    /// (MSC4291), where the room ID *is* the create event's ID.
    pub event_format_requires_room_create_room_id: bool,
    /// Whether `m.room.create` is allowed to appear in its own `auth_events`. Dropped from room
    /// version 12, where it structurally cannot (there is nothing to reference yet).
    pub event_format_allows_room_create_in_auth_events: bool,

    // --- state resolution ---
    /// The state resolution algorithm and its tweaks.
    pub state_res: StateResolutionVersion,

    // --- signatures ---
    /// Enforce the signing-key validity period. Added in room version 5.
    pub enforce_key_validity: bool,
    /// Check that the event ID's embedded server name matches the origin. Dropped from room
    /// version 3, where event IDs are opaque hashes.
    pub check_event_id_server: bool,
    /// Check the server named in `content.join_authorised_via_users_server`. Added in room
    /// version 8 (restricted joins).
    pub check_join_authorised_via_users_server: bool,

    // --- auth rules ---
    /// Apply the special-cased `m.room.redaction` auth rule (event always accepted, redaction
    /// applied only if the redacter had power). Dropped from room version 3, where redactions
    /// are authorized like any other event and applied leniently by the recipient.
    pub special_case_room_redaction: bool,
    /// Apply the special-cased `m.room.aliases` auth rule (only the state key's own server may
    /// send it). Dropped from room version 6.
    pub special_case_room_aliases: bool,
    /// Reject events whose JSON fails strict canonical JSON (floats, out-of-range integers).
    /// Added in room version 6. See [`crate::canonical`].
    pub strict_canonical_json: bool,
    /// Validate the `notifications` map in `m.room.power_levels`. Added in room version 6.
    pub limit_notifications_power_levels: bool,
    /// Allow `knock` as a membership and a join rule. Added in room version 7.
    pub knocking: bool,
    /// Allow the `restricted` join rule. Added in room version 8.
    pub restricted_join_rule: bool,
    /// Allow the `knock_restricted` join rule. Added in room version 10.
    pub knock_restricted_join_rule: bool,
    /// Require `m.room.power_levels` values to be integers (not integer strings or floats).
    /// Added in room version 10.
    pub integer_power_levels: bool,
    /// Determine the room creator from `m.room.create`'s `sender`, instead of
    /// `content.creator`. Added in room version 11.
    pub use_room_create_sender: bool,
    /// Room creators (and `content.additional_creators`) always have effectively infinite power
    /// level, regardless of `m.room.power_levels`. Added in room version 12 (MSC4289, "creator
    /// power").
    pub explicitly_privilege_room_creators: bool,
    /// Allow `content.additional_creators` on `m.room.create` to name co-creators. Added in room
    /// version 12 (MSC4289).
    pub additional_room_creators: bool,
    /// The room ID is the event ID of `m.room.create` (a content hash), rather than an opaque
    /// `!localpart:server` string. Added in room version 12 (MSC4291, "hash-based room IDs").
    pub room_create_event_id_as_room_id: bool,

    // --- redaction ---
    /// The redaction algorithm's tweaks.
    pub redaction: RedactionRules,
}

impl RoomVersionRules {
    /// Room version 1.
    pub const V1: Self = Self {
        disposition: RoomVersionDisposition::Stable,
        event_id_format: EventIdFormat::V1Opaque,
        room_id_format: RoomIdFormat::V1Opaque,
        events_reference_format: EventsReferenceFormat::V1WithHash,
        event_format_requires_event_id: true,
        event_format_requires_room_create_room_id: true,
        event_format_allows_room_create_in_auth_events: true,
        state_res: StateResolutionVersion::V1,
        enforce_key_validity: false,
        check_event_id_server: true,
        check_join_authorised_via_users_server: false,
        special_case_room_redaction: true,
        special_case_room_aliases: true,
        strict_canonical_json: false,
        limit_notifications_power_levels: false,
        knocking: false,
        restricted_join_rule: false,
        knock_restricted_join_rule: false,
        integer_power_levels: false,
        use_room_create_sender: false,
        explicitly_privilege_room_creators: false,
        additional_room_creators: false,
        room_create_event_id_as_room_id: false,
        redaction: RedactionRules::V1,
    };

    /// Room version 2: state resolution v2.
    pub const V2: Self = Self {
        state_res: StateResolutionVersion::V2 { v2_1: false },
        ..Self::V1
    };

    /// Room version 3: event ID and reference format change to content hashes.
    pub const V3: Self = Self {
        event_id_format: EventIdFormat::V2StandardBase64,
        events_reference_format: EventsReferenceFormat::V2IdOnly,
        event_format_requires_event_id: false,
        check_event_id_server: false,
        special_case_room_redaction: false,
        ..Self::V2
    };

    /// Room version 4: event IDs move to URL-safe base64.
    pub const V4: Self = Self {
        event_id_format: EventIdFormat::V3UrlSafeBase64,
        ..Self::V3
    };

    /// Room version 5: signing-key validity period enforced.
    pub const V5: Self = Self {
        enforce_key_validity: true,
        ..Self::V4
    };

    /// Room version 6: strict canonical JSON, `m.room.aliases` no longer special-cased.
    pub const V6: Self = Self {
        special_case_room_aliases: false,
        strict_canonical_json: true,
        limit_notifications_power_levels: true,
        redaction: RedactionRules::V6,
        ..Self::V5
    };

    /// Room version 7: knocking.
    pub const V7: Self = Self {
        knocking: true,
        ..Self::V6
    };

    /// Room version 8: restricted joins.
    pub const V8: Self = Self {
        restricted_join_rule: true,
        check_join_authorised_via_users_server: true,
        redaction: RedactionRules::V8,
        ..Self::V7
    };

    /// Room version 9: `join_authorised_via_users_server` survives redaction.
    pub const V9: Self = Self {
        redaction: RedactionRules::V9,
        ..Self::V8
    };

    /// Room version 10: `knock_restricted`, integer power levels.
    pub const V10: Self = Self {
        knock_restricted_join_rule: true,
        integer_power_levels: true,
        ..Self::V9
    };

    /// Room version 11: creator from `sender`, new redaction rules.
    pub const V11: Self = Self {
        use_room_create_sender: true,
        redaction: RedactionRules::V11,
        ..Self::V10
    };

    /// Room version 12: creator power (MSC4289) and hash-based room IDs (MSC4291).
    pub const V12: Self = Self {
        room_id_format: RoomIdFormat::V2HashBased,
        event_format_requires_room_create_room_id: false,
        event_format_allows_room_create_in_auth_events: false,
        state_res: StateResolutionVersion::V2 { v2_1: true },
        explicitly_privilege_room_creators: true,
        additional_room_creators: true,
        room_create_event_id_as_room_id: true,
        ..Self::V11
    };
}

/// A room version identifier paired with its rules.
#[derive(Debug, Clone, Copy)]
pub struct RoomVersion {
    /// The rules for this room version.
    pub rules: RoomVersionRules,
}

/// The room versions and MSC-gated identifiers this server knows, in the order
/// `docs/synapse-inventory.md` lists them: `1` to `12`, then the unstable ones.
///
/// The unstable identifiers are Synapse test/MSC-gate room versions; each is documented with the
/// stable version whose rules it otherwise uses (best-effort: their exact semantics are defined
/// by their MSC, not by this table, and are not independently modeled here beyond that
/// inheritance -- see `docs/status/02-state-and-model.md` "Decisions made").
const KNOWN: &[(&str, RoomVersionRules)] = &[
    ("1", RoomVersionRules::V1),
    ("2", RoomVersionRules::V2),
    ("3", RoomVersionRules::V3),
    ("4", RoomVersionRules::V4),
    ("5", RoomVersionRules::V5),
    ("6", RoomVersionRules::V6),
    ("7", RoomVersionRules::V7),
    ("8", RoomVersionRules::V8),
    ("9", RoomVersionRules::V9),
    ("10", RoomVersionRules::V10),
    ("11", RoomVersionRules::V11),
    ("12", RoomVersionRules::V12),
    // Synapse's "hydra" test room version: room version 11 rules, used to trial the explicit
    // state DAG ideas from the Project Hydra post ahead of MSC4297/MSC4242 landing on a numbered
    // version.
    ("org.matrix.hydra.11", unstable(RoomVersionRules::V11)),
    // MSC1767 (extensible events) trialled on room version 10's rules.
    ("org.matrix.msc1767.10", unstable(RoomVersionRules::V10)),
    // MSC3389 trialled on room version 10's rules.
    ("org.matrix.msc3389.10", unstable(RoomVersionRules::V10)),
    // MSC3757 (restricting who can redact) trialled on room versions 10 and 11's rules.
    ("org.matrix.msc3757.10", unstable(RoomVersionRules::V10)),
    ("org.matrix.msc3757.11", unstable(RoomVersionRules::V11)),
    // MSC4242 (explicit state DAG / `prev_state_events`) trialled on room version 12's rules.
    ("org.matrix.msc4242.12", unstable(RoomVersionRules::V12)),
];

const fn unstable(rules: RoomVersionRules) -> RoomVersionRules {
    RoomVersionRules {
        disposition: RoomVersionDisposition::Unstable,
        ..rules
    }
}

/// Looks up the rules for a room version identifier.
///
/// Returns `None` for identifiers this table does not know, matching
/// `RoomVersionId::rules()` returning `None` for Ruma's own unrecognized `_Custom` case.
#[must_use]
pub fn rules_for(id: &RoomVersionId) -> Option<RoomVersionRules> {
    let s = id.as_str();
    KNOWN
        .iter()
        .find(|(name, _)| *name == s)
        .map(|(_, rules)| *rules)
}

/// The identifiers this table knows, in table order.
pub fn known_room_version_ids() -> impl Iterator<Item = &'static str> {
    KNOWN.iter().map(|(name, _)| *name)
}

impl RoomVersion {
    /// Looks up a room version by identifier.
    ///
    /// # Errors
    /// Returns [`crate::error::EventError::UnknownRoomVersion`] if `id` is not in the table.
    pub fn lookup(id: &RoomVersionId) -> Result<Self, crate::error::EventError> {
        rules_for(id)
            .map(|rules| Self { rules })
            .ok_or_else(|| crate::error::EventError::UnknownRoomVersion(id.as_str().to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_versions_match_synapse_inventory() {
        // docs/synapse-inventory.md: "Room versions known to Synapse (18)".
        let ids: Vec<_> = known_room_version_ids().collect();
        assert_eq!(ids.len(), 18);
        for v in [
            "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12",
        ] {
            assert!(ids.contains(&v), "missing stable version {v}");
        }
        for v in [
            "org.matrix.hydra.11",
            "org.matrix.msc1767.10",
            "org.matrix.msc3389.10",
            "org.matrix.msc3757.10",
            "org.matrix.msc3757.11",
            "org.matrix.msc4242.12",
        ] {
            assert!(ids.contains(&v), "missing unstable version {v}");
        }
    }

    #[test]
    fn unknown_version_is_none() {
        let id = RoomVersionId::try_from("not-a-real-version").unwrap();
        assert!(rules_for(&id).is_none());
        assert!(RoomVersion::lookup(&id).is_err());
    }

    #[test]
    fn v12_has_creator_power_and_hash_room_ids() {
        let rules = rules_for(&RoomVersionId::V12).unwrap();
        assert!(rules.explicitly_privilege_room_creators);
        assert!(rules.additional_room_creators);
        assert!(rules.room_create_event_id_as_room_id);
        assert_eq!(rules.room_id_format, RoomIdFormat::V2HashBased);
        assert_eq!(rules.state_res, StateResolutionVersion::V2 { v2_1: true });
    }

    #[test]
    fn v1_uses_state_res_v1() {
        assert_eq!(
            rules_for(&RoomVersionId::V1).unwrap().state_res,
            StateResolutionVersion::V1
        );
    }

    /// Field-for-field agreement with `ruma_common::room_version_rules` for the stable versions,
    /// on every flag both tables model.
    #[test]
    fn cross_check_against_ruma_for_stable_versions() {
        use ruma::room_version_rules::{
            RoomVersionDisposition as RumaDisp, StateResolutionVersion as RumaStateRes,
        };

        let versions = [
            RoomVersionId::V1,
            RoomVersionId::V2,
            RoomVersionId::V3,
            RoomVersionId::V4,
            RoomVersionId::V5,
            RoomVersionId::V6,
            RoomVersionId::V7,
            RoomVersionId::V8,
            RoomVersionId::V9,
            RoomVersionId::V10,
            RoomVersionId::V11,
            RoomVersionId::V12,
        ];
        for id in versions {
            let ours = rules_for(&id).unwrap_or_else(|| panic!("missing rules for {id}"));
            let theirs = id
                .rules()
                .unwrap_or_else(|| panic!("ruma missing rules for {id}"));

            assert_eq!(
                ours.disposition == RoomVersionDisposition::Stable,
                theirs.disposition == RumaDisp::Stable,
                "disposition mismatch for {id}"
            );
            assert_eq!(
                ours.enforce_key_validity, theirs.enforce_key_validity,
                "enforce_key_validity mismatch for {id}"
            );
            assert_eq!(
                ours.check_event_id_server, theirs.signatures.check_event_id_server,
                "check_event_id_server mismatch for {id}"
            );
            assert_eq!(
                ours.check_join_authorised_via_users_server,
                theirs.signatures.check_join_authorised_via_users_server,
                "check_join_authorised_via_users_server mismatch for {id}"
            );
            assert_eq!(
                ours.special_case_room_redaction, theirs.authorization.special_case_room_redaction,
                "special_case_room_redaction mismatch for {id}"
            );
            assert_eq!(
                ours.special_case_room_aliases, theirs.authorization.special_case_room_aliases,
                "special_case_room_aliases mismatch for {id}"
            );
            assert_eq!(
                ours.strict_canonical_json, theirs.authorization.strict_canonical_json,
                "strict_canonical_json mismatch for {id}"
            );
            assert_eq!(
                ours.limit_notifications_power_levels,
                theirs.authorization.limit_notifications_power_levels,
                "limit_notifications_power_levels mismatch for {id}"
            );
            assert_eq!(
                ours.knocking, theirs.authorization.knocking,
                "knocking mismatch for {id}"
            );
            assert_eq!(
                ours.restricted_join_rule, theirs.authorization.restricted_join_rule,
                "restricted_join_rule mismatch for {id}"
            );
            assert_eq!(
                ours.knock_restricted_join_rule, theirs.authorization.knock_restricted_join_rule,
                "knock_restricted_join_rule mismatch for {id}"
            );
            assert_eq!(
                ours.integer_power_levels, theirs.authorization.integer_power_levels,
                "integer_power_levels mismatch for {id}"
            );
            assert_eq!(
                ours.use_room_create_sender, theirs.authorization.use_room_create_sender,
                "use_room_create_sender mismatch for {id}"
            );
            assert_eq!(
                ours.explicitly_privilege_room_creators,
                theirs.authorization.explicitly_privilege_room_creators,
                "explicitly_privilege_room_creators mismatch for {id}"
            );
            assert_eq!(
                ours.additional_room_creators, theirs.authorization.additional_room_creators,
                "additional_room_creators mismatch for {id}"
            );
            assert_eq!(
                ours.room_create_event_id_as_room_id,
                theirs.authorization.room_create_event_id_as_room_id,
                "room_create_event_id_as_room_id mismatch for {id}"
            );

            assert_eq!(
                ours.redaction.keep_room_aliases_aliases,
                theirs.redaction.keep_room_aliases_aliases,
                "keep_room_aliases_aliases mismatch for {id}"
            );
            assert_eq!(
                ours.redaction.keep_room_join_rules_allow,
                theirs.redaction.keep_room_join_rules_allow,
                "keep_room_join_rules_allow mismatch for {id}"
            );
            assert_eq!(
                ours.redaction
                    .keep_room_member_join_authorised_via_users_server,
                theirs
                    .redaction
                    .keep_room_member_join_authorised_via_users_server,
                "keep_room_member_join_authorised_via_users_server mismatch for {id}"
            );
            assert_eq!(
                ours.redaction.keep_origin_membership_prev_state,
                theirs.redaction.keep_origin_membership_prev_state,
                "keep_origin_membership_prev_state mismatch for {id}"
            );
            assert_eq!(
                ours.redaction.keep_room_create_content, theirs.redaction.keep_room_create_content,
                "keep_room_create_content mismatch for {id}"
            );
            assert_eq!(
                ours.redaction.keep_room_redaction_redacts,
                theirs.redaction.keep_room_redaction_redacts,
                "keep_room_redaction_redacts mismatch for {id}"
            );
            assert_eq!(
                ours.redaction.keep_room_power_levels_invite,
                theirs.redaction.keep_room_power_levels_invite,
                "keep_room_power_levels_invite mismatch for {id}"
            );
            assert_eq!(
                ours.redaction.keep_room_member_third_party_invite_signed,
                theirs.redaction.keep_room_member_third_party_invite_signed,
                "keep_room_member_third_party_invite_signed mismatch for {id}"
            );
            assert_eq!(
                ours.redaction.content_field_redacts, theirs.redaction.content_field_redacts,
                "content_field_redacts mismatch for {id}"
            );

            match (ours.state_res, &theirs.state_res) {
                (StateResolutionVersion::V1, RumaStateRes::V1) => {}
                (StateResolutionVersion::V2 { v2_1 }, RumaStateRes::V2(v2_rules)) => {
                    assert_eq!(
                        v2_1, v2_rules.begin_iterative_auth_checks_with_empty_state_map,
                        "state_res v2.1 flag mismatch for {id}"
                    );
                }
                _ => panic!("state_res kind mismatch for {id}"),
            }
        }
    }
}
