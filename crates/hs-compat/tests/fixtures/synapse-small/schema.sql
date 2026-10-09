-- The part of a Synapse database (schema version 94, Synapse 1.161) that the importer reads:
-- only the tables and columns `hs_compat::migration::source` queries, written for this fixture
-- from the column names in docs/compat/synapse-importer-mapping.md. It is not Synapse's schema
-- and nothing here is copied from it. The rows in data.sql were produced by a real Synapse
-- (see README.md).

CREATE TABLE users (
    name text PRIMARY KEY,
    password_hash text,
    creation_ts bigint,
    admin smallint NOT NULL DEFAULT 0,
    is_guest smallint NOT NULL DEFAULT 0,
    appservice_id text,
    user_type text,
    deactivated smallint NOT NULL DEFAULT 0,
    shadow_banned boolean,
    locked boolean NOT NULL DEFAULT false
);

CREATE TABLE profiles (
    user_id text NOT NULL,
    displayname text,
    avatar_url text,
    full_user_id text
);

CREATE TABLE devices (
    user_id text NOT NULL,
    device_id text NOT NULL,
    display_name text,
    last_seen bigint,
    ip text,
    hidden boolean DEFAULT false,
    PRIMARY KEY (user_id, device_id)
);

CREATE TABLE access_tokens (
    id bigint PRIMARY KEY,
    user_id text NOT NULL,
    device_id text,
    token text NOT NULL,
    valid_until_ms bigint,
    puppets_user_id text,
    refresh_token_id bigint
);

CREATE TABLE refresh_tokens (
    id bigint PRIMARY KEY,
    user_id text NOT NULL,
    device_id text NOT NULL,
    token text NOT NULL,
    next_token_id bigint,
    expiry_ts bigint,
    ultimate_session_expiry_ts bigint
);

CREATE TABLE user_threepids (
    user_id text NOT NULL,
    medium text NOT NULL,
    address text NOT NULL,
    validated_at bigint NOT NULL,
    added_at bigint NOT NULL
);

CREATE TABLE user_external_ids (
    auth_provider text NOT NULL,
    external_id text NOT NULL,
    user_id text NOT NULL
);

CREATE TABLE erased_users (
    user_id text NOT NULL
);

CREATE TABLE device_inbox (
    user_id text NOT NULL,
    device_id text NOT NULL,
    stream_id bigint NOT NULL,
    message_json text NOT NULL,
    instance_name text
);

CREATE TABLE registration_tokens (
    token text NOT NULL,
    uses_allowed integer,
    pending integer NOT NULL,
    completed integer NOT NULL,
    expiry_time bigint
);

CREATE TABLE account_data (
    user_id text NOT NULL,
    account_data_type text NOT NULL,
    stream_id bigint NOT NULL,
    content text NOT NULL
);

CREATE TABLE room_account_data (
    user_id text NOT NULL,
    room_id text NOT NULL,
    account_data_type text NOT NULL,
    stream_id bigint NOT NULL,
    content text NOT NULL
);

CREATE TABLE room_tags (
    user_id text NOT NULL,
    room_id text NOT NULL,
    tag text NOT NULL,
    content text NOT NULL
);

CREATE TABLE rooms (
    room_id text PRIMARY KEY,
    is_public boolean,
    creator text,
    room_version text
);

CREATE TABLE room_aliases (
    room_alias text PRIMARY KEY,
    room_id text NOT NULL,
    creator text
);

CREATE TABLE events (
    event_id text PRIMARY KEY,
    room_id text NOT NULL,
    type text NOT NULL,
    state_key text,
    sender text,
    depth bigint NOT NULL,
    topological_ordering bigint,
    stream_ordering bigint,
    outlier boolean NOT NULL,
    origin_server_ts bigint,
    rejection_reason text
);

CREATE TABLE event_json (
    event_id text PRIMARY KEY,
    room_id text NOT NULL,
    json text NOT NULL,
    format_version integer
);

CREATE TABLE rejections (
    event_id text PRIMARY KEY,
    reason text NOT NULL
);

CREATE TABLE redactions (
    event_id text PRIMARY KEY,
    redacts text NOT NULL,
    have_censored boolean NOT NULL DEFAULT false
);

CREATE TABLE current_state_events (
    event_id text NOT NULL,
    room_id text NOT NULL,
    type text NOT NULL,
    state_key text NOT NULL,
    membership text
);

CREATE TABLE local_media_repository (
    media_id text PRIMARY KEY,
    media_type text,
    media_length integer,
    created_ts bigint,
    upload_name text,
    user_id text,
    quarantined_by text,
    safe_from_quarantine boolean NOT NULL DEFAULT false
);

CREATE TABLE e2e_device_keys_json (
    user_id text NOT NULL,
    device_id text NOT NULL,
    ts_added_ms bigint NOT NULL,
    key_json text NOT NULL
);

CREATE TABLE e2e_one_time_keys_json (
    user_id text NOT NULL,
    device_id text NOT NULL,
    algorithm text NOT NULL,
    key_id text NOT NULL,
    ts_added_ms bigint NOT NULL,
    key_json text NOT NULL
);

CREATE TABLE e2e_fallback_keys_json (
    user_id text NOT NULL,
    device_id text NOT NULL,
    algorithm text NOT NULL,
    key_id text NOT NULL,
    key_json text NOT NULL,
    used boolean NOT NULL DEFAULT false
);

CREATE TABLE e2e_cross_signing_keys (
    user_id text NOT NULL,
    keytype text NOT NULL,
    keydata text NOT NULL,
    stream_id bigint NOT NULL
);

CREATE TABLE e2e_cross_signing_signatures (
    user_id text NOT NULL,
    key_id text NOT NULL,
    target_user_id text NOT NULL,
    target_device_id text NOT NULL,
    signature text NOT NULL
);

CREATE TABLE e2e_room_keys_versions (
    user_id text NOT NULL,
    version bigint NOT NULL,
    algorithm text NOT NULL,
    auth_data text NOT NULL,
    deleted smallint NOT NULL DEFAULT 0,
    etag bigint
);

CREATE TABLE e2e_room_keys (
    user_id text NOT NULL,
    room_id text NOT NULL,
    session_id text NOT NULL,
    version bigint NOT NULL,
    first_message_index integer,
    forwarded_count integer,
    is_verified boolean,
    session_data text NOT NULL
);

CREATE TABLE push_rules (
    id bigint NOT NULL,
    user_name text NOT NULL,
    rule_id text NOT NULL,
    priority_class smallint NOT NULL,
    priority integer NOT NULL DEFAULT 0,
    conditions text NOT NULL,
    actions text NOT NULL
);

CREATE TABLE push_rules_enable (
    id bigint NOT NULL,
    user_name text NOT NULL,
    rule_id text NOT NULL,
    enabled smallint
);

CREATE TABLE pushers (
    id bigint NOT NULL,
    user_name text NOT NULL,
    access_token bigint,
    profile_tag text NOT NULL,
    kind text NOT NULL,
    app_id text NOT NULL,
    app_display_name text NOT NULL,
    device_display_name text NOT NULL,
    pushkey text NOT NULL,
    ts bigint NOT NULL,
    lang text,
    data text,
    enabled boolean,
    device_id text
);

CREATE TABLE receipts_linearized (
    stream_id bigint NOT NULL,
    room_id text NOT NULL,
    receipt_type text NOT NULL,
    user_id text NOT NULL,
    event_id text NOT NULL,
    data text NOT NULL,
    thread_id text
);

CREATE TABLE user_filters (
    user_id text NOT NULL,
    full_user_id text,
    filter_id bigint NOT NULL,
    filter_json bytea NOT NULL
);

-- Other servers' media Synapse had cached (Synapse's remote_media_cache, with the columns later
-- deltas added: authenticated, sha256). Rows added by hand on 2026-10-04 (see README).
CREATE TABLE remote_media_cache (
    media_origin text,
    media_id text,
    media_type text,
    created_ts bigint,
    upload_name text,
    media_length integer,
    filesystem_id text,
    last_access_ts bigint,
    quarantined_by text,
    authenticated boolean NOT NULL DEFAULT false,
    sha256 text
);

CREATE TABLE remote_media_cache_thumbnails (
    media_origin text,
    media_id text,
    thumbnail_width integer,
    thumbnail_height integer,
    thumbnail_method text,
    thumbnail_type text,
    thumbnail_length integer,
    filesystem_id text
);
