CREATE TABLE change_sets (seq TEXT PRIMARY KEY NOT NULL, operation_id TEXT UNIQUE);

CREATE TABLE devices (
            device_id TEXT PRIMARY KEY NOT NULL,
            principal_id TEXT NOT NULL REFERENCES principals(principal_id),
            stream_id TEXT UNIQUE NOT NULL REFERENCES operation_streams(stream_id),
            token_id TEXT UNIQUE NOT NULL,
            token_hash BLOB NOT NULL CHECK(typeof(token_hash)='blob' AND length(token_hash)=32),
            issued_at INTEGER NOT NULL CHECK(typeof(issued_at)='integer' AND issued_at>=0),
            idle_expires_at INTEGER NOT NULL CHECK(typeof(idle_expires_at)='integer' AND idle_expires_at>issued_at),
            absolute_expires_at INTEGER NOT NULL CHECK(typeof(absolute_expires_at)='integer' AND absolute_expires_at>=idle_expires_at),
            revoked_at INTEGER CHECK(revoked_at IS NULL OR (typeof(revoked_at)='integer' AND revoked_at>=issued_at)));

CREATE TABLE node_changes (seq TEXT NOT NULL REFERENCES change_sets(seq),
                node_id TEXT NOT NULL REFERENCES nodes(node_id), post_state TEXT NOT NULL,
                PRIMARY KEY(seq,node_id));

CREATE TABLE nodes (
                node_id TEXT PRIMARY KEY NOT NULL, parent_id TEXT REFERENCES nodes(node_id),
                name TEXT NOT NULL, normalized_name_key TEXT NOT NULL, kind TEXT NOT NULL,
                content_rev TEXT, meta_rev TEXT NOT NULL, content_hash TEXT, size TEXT,
                children_generation TEXT NOT NULL, last_change_seq TEXT NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN(0,1)));

CREATE TABLE operation_receipts (
                operation_id TEXT PRIMARY KEY NOT NULL, stream_id TEXT NOT NULL REFERENCES operation_streams(stream_id),
                operation_seq TEXT NOT NULL, fingerprint TEXT NOT NULL, outcome TEXT NOT NULL,
                pending INTEGER NOT NULL CHECK(pending IN(0,1)), node_etag TEXT, UNIQUE(stream_id,operation_seq));

CREATE TABLE operation_streams (stream_id TEXT PRIMARY KEY NOT NULL, last_seq TEXT NOT NULL, retired_through TEXT NOT NULL DEFAULT '0');

CREATE TABLE pairing_secrets (
            pairing_id TEXT PRIMARY KEY NOT NULL,
            principal_id TEXT NOT NULL REFERENCES principals(principal_id),
            workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
            issued_role TEXT NOT NULL CHECK(issued_role IN ('reader','editor','owner')),
            secret_hash BLOB NOT NULL CHECK(typeof(secret_hash)='blob' AND length(secret_hash)=32),
            issued_at INTEGER NOT NULL CHECK(typeof(issued_at)='integer' AND issued_at>=0),
            expires_at INTEGER NOT NULL CHECK(typeof(expires_at)='integer' AND expires_at>issued_at),
            consumed_at INTEGER CHECK(consumed_at IS NULL OR (typeof(consumed_at)='integer' AND consumed_at>=issued_at AND consumed_at<expires_at)));

CREATE TABLE principals (
            principal_id TEXT PRIMARY KEY NOT NULL,
            workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
            workspace_role TEXT NOT NULL CHECK(workspace_role IN ('reader','editor','owner')));

CREATE TABLE recovery_cases (
        operation_id TEXT PRIMARY KEY NOT NULL, intent TEXT NOT NULL, paths TEXT NOT NULL,
        resolved INTEGER NOT NULL DEFAULT 0 CHECK(resolved IN(0,1)), resolved_head TEXT);

CREATE TABLE recovery_objects (
        operation_id TEXT NOT NULL REFERENCES recovery_cases(operation_id) ON DELETE CASCADE,
        hash TEXT NOT NULL, PRIMARY KEY(operation_id,hash));

CREATE TABLE retired_operations (
             operation_id TEXT PRIMARY KEY NOT NULL,
             stream_id TEXT NOT NULL REFERENCES operation_streams(stream_id),
             operation_seq TEXT NOT NULL,
             UNIQUE(stream_id,operation_seq));

CREATE TABLE uploads (
        upload_id TEXT PRIMARY KEY NOT NULL, device_id TEXT NOT NULL REFERENCES devices(device_id),
        status TEXT NOT NULL CHECK(status IN('receiving','ready','consumed')),
        expected_hash TEXT NOT NULL, declared_size INTEGER, reserved_bytes INTEGER NOT NULL,
        started_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, content_hash TEXT, size INTEGER,
        pending_operation_id TEXT UNIQUE REFERENCES write_intents(operation_id) ON DELETE SET NULL);

CREATE TABLE workspaces (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), workspace_id TEXT NOT NULL,
            epoch TEXT NOT NULL, root_id TEXT NOT NULL, head_seq TEXT NOT NULL);

CREATE TABLE write_intents (
                operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operation_receipts(operation_id),
                intent TEXT NOT NULL, observed_hash TEXT, recovery_conflict INTEGER NOT NULL DEFAULT 0);

CREATE UNIQUE INDEX auth_one_owner ON principals(workspace_id) WHERE workspace_role='owner';

CREATE UNIQUE INDEX auth_workspace_id ON workspaces(workspace_id);

CREATE UNIQUE INDEX live_node_name ON nodes(parent_id, normalized_name_key) WHERE deleted=0;

PRAGMA application_id=1314148947;
PRAGMA user_version=9;
