-- The registry's tables in D1 (worker.js). Archives are in KV, by
-- `archive/<name>/<version>`.

-- Accounts, by a hash of their tokens; the tokens themselves are never kept.
CREATE TABLE IF NOT EXISTS users (
    name TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    created TEXT NOT NULL
);

-- Packages, each owned by the account that published it first.
CREATE TABLE IF NOT EXISTS packages (
    name TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES users (name),
    description TEXT NOT NULL DEFAULT '',
    latest TEXT NOT NULL,
    created TEXT NOT NULL,
    updated TEXT NOT NULL
);

-- Each published version, which never changes: its archive's digest and
-- size, and its dependencies on registry packages as JSON.
CREATE TABLE IF NOT EXISTS versions (
    name TEXT NOT NULL REFERENCES packages (name),
    version TEXT NOT NULL,
    digest TEXT NOT NULL,
    size INTEGER NOT NULL,
    dependencies TEXT NOT NULL,
    published TEXT NOT NULL,
    publisher TEXT NOT NULL,
    PRIMARY KEY (name, version)
);
