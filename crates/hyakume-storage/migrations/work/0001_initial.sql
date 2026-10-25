-- Shared by local.db and server.db. Local work uses the same tables with a
-- single local identity, so both run the same queries and cannot drift apart.
-- IDs are UUID text; timestamps are Unix milliseconds.

CREATE TABLE identities (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    identity_id TEXT NOT NULL REFERENCES identities (id) ON DELETE CASCADE,
    -- SHA-256 of the bearer token; the token itself is never stored.
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL
) STRICT;

CREATE INDEX sessions_identity ON sessions (identity_id);

CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE memberships (
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    identity_id TEXT NOT NULL REFERENCES identities (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('member', 'guest')),
    is_owner INTEGER NOT NULL DEFAULT 0 CHECK (is_owner IN (0, 1)),
    PRIMARY KEY (workspace_id, identity_id)
) STRICT;

CREATE UNIQUE INDEX memberships_one_owner ON memberships (workspace_id) WHERE is_owner = 1;
CREATE INDEX memberships_identity ON memberships (identity_id);

CREATE TABLE roles (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    preset TEXT CHECK (preset IN ('owner', 'admin', 'member', 'guest')),
    UNIQUE (workspace_id, name)
) STRICT;

CREATE TABLE role_permissions (
    role_id TEXT NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
    permission TEXT NOT NULL,
    PRIMARY KEY (role_id, permission)
) STRICT;

CREATE TABLE membership_roles (
    workspace_id TEXT NOT NULL,
    identity_id TEXT NOT NULL,
    role_id TEXT NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
    PRIMARY KEY (workspace_id, identity_id, role_id),
    FOREIGN KEY (workspace_id, identity_id)
        REFERENCES memberships (workspace_id, identity_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    name TEXT NOT NULL,
    next_task_number INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    UNIQUE (workspace_id, key)
) STRICT;

CREATE TABLE guest_projects (
    workspace_id TEXT NOT NULL,
    identity_id TEXT NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    PRIMARY KEY (workspace_id, identity_id, project_id),
    FOREIGN KEY (workspace_id, identity_id)
        REFERENCES memberships (workspace_id, identity_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE stages (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    position INTEGER NOT NULL,
    finished INTEGER NOT NULL CHECK (finished IN (0, 1)),
    UNIQUE (project_id, name)
) STRICT;

CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects (id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    title TEXT NOT NULL,
    description TEXT NOT NULL,
    stage_id TEXT NOT NULL REFERENCES stages (id),
    assignee_id TEXT REFERENCES identities (id) ON DELETE SET NULL,
    -- JSON array of strings.
    labels TEXT NOT NULL DEFAULT '[]',
    priority TEXT CHECK (priority IN ('low', 'medium', 'high', 'urgent')),
    -- ISO 8601 date, YYYY-MM-DD.
    due_date TEXT,
    version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    created_by TEXT REFERENCES identities (id) ON DELETE SET NULL,
    updated_by TEXT REFERENCES identities (id) ON DELETE SET NULL,
    UNIQUE (project_id, number)
) STRICT;

CREATE INDEX tasks_stage ON tasks (stage_id);
CREATE INDEX tasks_assignee ON tasks (assignee_id);

CREATE TABLE comments (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    author_id TEXT REFERENCES identities (id) ON DELETE SET NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX comments_task ON comments (task_id);

CREATE TABLE relation_types (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    reverse_name TEXT,
    directed INTEGER NOT NULL CHECK (directed IN (0, 1)),
    builtin TEXT CHECK (builtin IN ('blocks', 'related_to')),
    UNIQUE (workspace_id, name),
    UNIQUE (workspace_id, builtin)
) STRICT;

CREATE TABLE relations (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    type_id TEXT NOT NULL REFERENCES relation_types (id) ON DELETE CASCADE,
    from_task TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    to_task TEXT NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    created_by TEXT REFERENCES identities (id) ON DELETE SET NULL,
    UNIQUE (type_id, from_task, to_task),
    CHECK (from_task <> to_task)
) STRICT;

CREATE INDEX relations_from ON relations (from_task);
CREATE INDEX relations_to ON relations (to_task);

-- Personal canvas positions. Rows belong to one identity and are never sent
-- to anyone else.
CREATE TABLE layouts (
    identity_id TEXT NOT NULL REFERENCES identities (id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    node_kind TEXT NOT NULL CHECK (node_kind IN ('project', 'task')),
    node_id TEXT NOT NULL,
    x REAL NOT NULL,
    y REAL NOT NULL,
    pinned INTEGER NOT NULL CHECK (pinned IN (0, 1)),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (identity_id, node_kind, node_id)
) STRICT;

CREATE INDEX layouts_workspace ON layouts (identity_id, workspace_id);
