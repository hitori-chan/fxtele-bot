-- Small key-value documents: live settings, cookies, Telegram ranges, the password hash.
CREATE TABLE kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- Request counts by outcome and page type, in one-minute (size 60) and hourly (size 3600) buckets.
CREATE TABLE stats (
    size INTEGER NOT NULL,
    bucket INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    page_type TEXT NOT NULL,
    count INTEGER NOT NULL,
    total_ms INTEGER NOT NULL,
    max_ms INTEGER NOT NULL,
    -- Time first: the dashboard reads recent rows of both sizes.
    PRIMARY KEY (bucket, size, outcome, page_type)
) STRICT, WITHOUT ROWID;

-- The most recent failed extractions.
CREATE TABLE failures (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    url TEXT NOT NULL,
    page_type TEXT NOT NULL,
    stage TEXT NOT NULL,
    error TEXT NOT NULL
) STRICT;

CREATE TABLE passkeys (
    id BLOB PRIMARY KEY,
    name TEXT NOT NULL,
    passkey TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
) STRICT;

-- Dashboard sessions; only a hash of the cookie's token is stored.
CREATE TABLE sessions (
    token_hash BLOB PRIMARY KEY,
    method TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    ip TEXT NOT NULL,
    user_agent TEXT NOT NULL
) STRICT;

-- One-time setup links; only a hash of the token is stored.
CREATE TABLE setup_links (
    token_hash BLOB PRIMARY KEY,
    expires_at INTEGER NOT NULL
) STRICT;

-- Last result of each scheduled test link.
CREATE TABLE canary_results (
    url TEXT PRIMARY KEY,
    checked_at INTEGER NOT NULL,
    ok INTEGER NOT NULL,
    page_type TEXT NOT NULL,
    detail TEXT NOT NULL
) STRICT;
