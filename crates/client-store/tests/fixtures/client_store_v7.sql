-- Synthetic snapshot of the v7 schema, before the outbox migration.
CREATE TABLE draft_checkpoints (
                    document_id TEXT PRIMARY KEY NOT NULL,
                    generation INTEGER NOT NULL CHECK (generation > 0),
                    format_version INTEGER NOT NULL CHECK (format_version = 1),
                    bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880)
                );
                CREATE TABLE draft_bases (
                    document_id TEXT PRIMARY KEY NOT NULL REFERENCES draft_checkpoints(document_id),
                    bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
                    revision TEXT NOT NULL
                );
                PRAGMA user_version = 1;
                PRAGMA application_id = 1314148952;

    CREATE TABLE client_registrations (
        id INTEGER PRIMARY KEY,
        server_origin TEXT NOT NULL,
        workspace_id TEXT NOT NULL,
        workspace_epoch TEXT NOT NULL,
        device_id TEXT NOT NULL,
        device_stream_id TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('active', 'frozen')),
        UNIQUE (server_origin, workspace_id, workspace_epoch, device_id, device_stream_id)
    );
    CREATE UNIQUE INDEX one_active_registration ON client_registrations(status) WHERE status = 'active';
    CREATE TABLE registered_documents (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        document_id TEXT NOT NULL,
        storage_id TEXT NOT NULL UNIQUE,
        PRIMARY KEY (registration_id, document_id)
    );
    PRAGMA user_version = 2;


    CREATE TABLE outbox_streams (
        registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id),
        next_seq INTEGER NOT NULL CHECK (next_seq > 0),
        ack_through INTEGER NOT NULL CHECK (ack_through >= 0)
    );
    CREATE TABLE client_outbox (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        document_id TEXT NOT NULL,
        storage_id TEXT NOT NULL REFERENCES registered_documents(storage_id),
        generation INTEGER NOT NULL CHECK (generation > 0),
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        UNIQUE(registration_id, operation_seq)
    );
    PRAGMA user_version = 3;


    ALTER TABLE client_outbox ADD COLUMN abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent'));
    ALTER TABLE client_outbox ADD COLUMN terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied'));
    PRAGMA user_version = 4;


    ALTER TABLE outbox_streams ADD COLUMN confirmed_through INTEGER NOT NULL DEFAULT 0 CHECK (confirmed_through >= 0);
    ALTER TABLE client_outbox ADD COLUMN payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1));
    ALTER TABLE client_outbox ADD COLUMN payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880);
    UPDATE client_outbox SET payload_len = length(bytes);
    CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
    CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
    CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
    PRAGMA user_version = 5;


    CREATE TABLE sync_bootstrap (
        registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id),
        session TEXT NOT NULL UNIQUE,
        root_id TEXT NOT NULL,
        h0 TEXT NOT NULL,
        h1 TEXT,
        cursor TEXT NOT NULL,
        inventory_after TEXT,
        inventory_done INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE bootstrap_nodes (
        registration_id INTEGER NOT NULL REFERENCES sync_bootstrap(registration_id) ON DELETE CASCADE,
        node_id TEXT NOT NULL,
        state TEXT NOT NULL,
        PRIMARY KEY(registration_id, node_id)
    );
    CREATE TABLE remote_index (
        registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id),
        root_id TEXT NOT NULL,
        cursor TEXT NOT NULL
    );
    CREATE TABLE remote_nodes (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        node_id TEXT NOT NULL,
        state TEXT NOT NULL,
        content_ready INTEGER NOT NULL DEFAULT 0 CHECK(content_ready IN (0, 1)),
        PRIMARY KEY(registration_id, node_id)
    );
    PRAGMA user_version=6;


    CREATE TABLE cached_content (
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        node_id TEXT NOT NULL,
        content_rev TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        size TEXT NOT NULL,
        bytes BLOB NOT NULL CHECK(length(bytes) <= 26214400),
        last_access INTEGER NOT NULL CHECK(last_access >= 0),
        PRIMARY KEY(registration_id, node_id)
    );
    CREATE INDEX cached_content_lru ON cached_content(last_access,registration_id,node_id);
    CREATE TABLE cached_content_clock (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        last_access INTEGER NOT NULL CHECK(last_access >= 0)
    );
    INSERT INTO cached_content_clock(singleton,last_access) VALUES(1,0);
    PRAGMA user_version = 7;
