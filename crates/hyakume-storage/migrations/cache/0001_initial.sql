-- The last copy of each shared workspace a client loaded, so a restart
-- while its server is unreachable still shows that work, read-only. The
-- body is whatever JSON the client wrote; this file never holds local work.

CREATE TABLE workspaces (
    -- Server URL and identity, so a server renamed in client.toml, or
    -- another person's sign-in, never shows someone else's copy.
    source TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    body TEXT NOT NULL,
    saved_at INTEGER NOT NULL,
    PRIMARY KEY (source, workspace_id)
) STRICT;
