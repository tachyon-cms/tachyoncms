-- Signing in: roles, the accounts behind admin users, their sessions and API keys.
-- Users themselves are documents in the `users` collection; only credentials live here,
-- so no document code path (filters, export, population) can ever reach them.

CREATE TABLE roles (
    tenant_id  uuid NOT NULL REFERENCES tenants (id),
    slug       text NOT NULL,
    name       text NOT NULL,
    builtin    boolean NOT NULL DEFAULT false,
    -- {"content": "all" | "own" | "read", "schemas": bool, "users": bool}
    abilities  jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, slug)
);

INSERT INTO roles (tenant_id, slug, name, builtin, abilities) VALUES
    ('00000000-0000-0000-0000-000000000001', 'admin',  'Admin',  true, '{"content": "all",  "schemas": true,  "users": true}'),
    ('00000000-0000-0000-0000-000000000001', 'editor', 'Editor', true, '{"content": "all",  "schemas": false, "users": false}'),
    ('00000000-0000-0000-0000-000000000001', 'author', 'Author', true, '{"content": "own",  "schemas": false, "users": false}'),
    ('00000000-0000-0000-0000-000000000001', 'viewer', 'Viewer', true, '{"content": "read", "schemas": false, "users": false}');

CREATE TABLE accounts (
    user_id             uuid PRIMARY KEY,
    tenant_id           uuid NOT NULL REFERENCES tenants (id),
    email               text NOT NULL,
    password_hash       text NOT NULL,
    role                text NOT NULL,
    failed_logins       integer NOT NULL DEFAULT 0,
    first_failure_at    timestamptz,
    locked_until        timestamptz,
    password_changed_at timestamptz NOT NULL DEFAULT now(),
    created_at          timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant_id, user_id) REFERENCES documents (tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, role) REFERENCES roles (tenant_id, slug)
);

CREATE UNIQUE INDEX accounts_email ON accounts (tenant_id, lower(email));
ALTER TABLE accounts ADD CONSTRAINT accounts_tenant_user_key UNIQUE (tenant_id, user_id);

CREATE TABLE sessions (
    id           uuid PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES tenants (id),
    user_id      uuid NOT NULL,
    -- SHA-256 of the token; the token itself is never stored.
    token_hash   bytea NOT NULL UNIQUE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    user_agent   text,
    ip           text,
    FOREIGN KEY (tenant_id, user_id) REFERENCES accounts (tenant_id, user_id) ON DELETE CASCADE
);

CREATE INDEX sessions_user ON sessions (user_id);

CREATE TABLE api_keys (
    id           uuid PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES tenants (id),
    user_id      uuid NOT NULL,
    name         text NOT NULL,
    -- Shown to people to tell keys apart; the secret is only ever stored hashed.
    prefix       text NOT NULL UNIQUE,
    secret_hash  bytea NOT NULL,
    scopes       text[] NOT NULL,
    expires_at   timestamptz,
    last_used_at timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now(),
    revoked_at   timestamptz,
    FOREIGN KEY (tenant_id, user_id) REFERENCES accounts (tenant_id, user_id) ON DELETE CASCADE
);

CREATE INDEX api_keys_user ON api_keys (user_id);
