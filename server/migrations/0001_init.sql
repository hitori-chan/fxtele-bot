-- Small key-value documents: live settings, cookies, Telegram ranges, the password hash.
CREATE TABLE kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- Request counts by outcome, source and page type, in one-minute (size 60) and hourly (size 3600)
-- buckets. `-` stands for no page (requests refused before one was read). The `under_*` columns
-- count requests by how long they took (each band starts where the one before ends), for
-- percentiles.
CREATE TABLE stats (
    size INTEGER NOT NULL,
    bucket INTEGER NOT NULL,
    source TEXT NOT NULL,
    outcome TEXT NOT NULL,
    page_type TEXT NOT NULL,
    count INTEGER NOT NULL,
    total_ms INTEGER NOT NULL,
    max_ms INTEGER NOT NULL,
    under_100ms INTEGER NOT NULL DEFAULT 0,
    under_250ms INTEGER NOT NULL DEFAULT 0,
    under_500ms INTEGER NOT NULL DEFAULT 0,
    under_1s INTEGER NOT NULL DEFAULT 0,
    under_2s INTEGER NOT NULL DEFAULT 0,
    under_4s INTEGER NOT NULL DEFAULT 0,
    under_8s INTEGER NOT NULL DEFAULT 0,
    -- Time first: the dashboard reads recent rows of both sizes.
    PRIMARY KEY (bucket, size, source, outcome, page_type)
) STRICT, WITHOUT ROWID;

-- What was fetched after the previews, bucketed like `stats`: `kind` is `image` (a photo or a
-- thumbnail), `video` (the site's file), `video_1080` (from the bucket), `fallback` (1080p wanted,
-- the site's file sent) or `mosaic`.
CREATE TABLE media (
    size INTEGER NOT NULL,
    bucket INTEGER NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY (bucket, size, source, kind)
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

-- The latest checks of each scheduled test link (48 each: a day's worth).
CREATE TABLE canary_results (
    url TEXT NOT NULL,
    checked_at INTEGER NOT NULL,
    ok INTEGER NOT NULL,
    page_type TEXT NOT NULL,
    detail TEXT NOT NULL,
    PRIMARY KEY (url, checked_at)
) STRICT, WITHOUT ROWID;

-- What was done on the dashboard, sign-ins, and alerts, newest kept. `kind` is `change`,
-- `sign_in` or `alert`; `actor` says who (how they signed in) or what.
CREATE TABLE activity (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    actor TEXT NOT NULL,
    text TEXT NOT NULL
) STRICT;
