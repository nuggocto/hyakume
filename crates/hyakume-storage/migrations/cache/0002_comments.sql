-- Comments of the shared tasks a client opened, so they still read offline.
-- Only opened tasks: fetching every comment with each load would make a
-- big workspace's refresh much heavier for a copy that's rarely read.

CREATE TABLE comments (
    source TEXT NOT NULL,
    task_id TEXT NOT NULL,
    body TEXT NOT NULL,
    saved_at INTEGER NOT NULL,
    PRIMARY KEY (source, task_id)
) STRICT;
