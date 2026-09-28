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
    puppets_user_id text
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
