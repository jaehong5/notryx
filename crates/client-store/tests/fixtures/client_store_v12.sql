-- Independent v12 schema supplement applied to the empty v7 fixture.
DROP TABLE client_outbox;
    CREATE TABLE client_outbox (
        operation_id TEXT PRIMARY KEY NOT NULL,
        registration_id INTEGER NOT NULL REFERENCES client_registrations(id),
        operation_seq INTEGER NOT NULL CHECK (operation_seq > 0),
        operation_kind TEXT NOT NULL DEFAULT 'content' CHECK (operation_kind IN ('content', 'restore', 'create', 'delete')),
        document_id TEXT NOT NULL,
        storage_id TEXT REFERENCES registered_documents(storage_id),
        generation INTEGER,
        expected_revision TEXT,
        dependency TEXT REFERENCES client_outbox(operation_id),
        bytes BLOB NOT NULL CHECK (length(bytes) <= 5242880),
        payload_hash TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'committed', 'conflict')),
        committed_revision TEXT,
        conflict_revision TEXT,
        conflict_etag TEXT,
        abandon_state TEXT NOT NULL DEFAULT 'none' CHECK (abandon_state IN ('none', 'requested', 'sent')),
        terminal_kind TEXT CHECK (terminal_kind IN ('abandoned', 'node_deleted', 'recovery_conflict', 'not_found', 'not_applied', 'not_deleted', 'history_expired', 'metadata_precondition_failed', 'destination_exists', 'parent_not_folder', 'node_id_conflict', 'node_precondition_failed')),
        payload_retired INTEGER NOT NULL DEFAULT 0 CHECK (payload_retired IN (0, 1)),
        payload_len INTEGER NOT NULL DEFAULT 0 CHECK (payload_len >= 0 AND payload_len <= 5242880),
        receipt_json TEXT CHECK (receipt_json IS NULL OR length(receipt_json) <= 65536),
        CHECK ((operation_kind = 'content' AND storage_id IS NOT NULL AND generation IS NOT NULL AND generation > 0)
            OR (operation_kind IN ('restore', 'create', 'delete') AND storage_id IS NULL AND generation IS NULL)),
        UNIQUE(registration_id, operation_seq)
    );
CREATE INDEX outbox_document_recent ON client_outbox(registration_id, document_id, operation_seq DESC);
CREATE INDEX outbox_document_status ON client_outbox(registration_id, document_id, status, terminal_kind);
CREATE INDEX outbox_dependencies ON client_outbox(registration_id, dependency, status);
        CREATE TABLE credential_pending (singleton INTEGER PRIMARY KEY CHECK(singleton=1), item_id TEXT NOT NULL UNIQUE, server_origin TEXT NOT NULL, workspace_id TEXT NOT NULL, workspace_epoch TEXT NOT NULL, device_id TEXT NOT NULL, device_stream_id TEXT NOT NULL, previous_registration_id INTEGER REFERENCES client_registrations(id));
        CREATE TABLE credential_links (registration_id INTEGER PRIMARY KEY REFERENCES client_registrations(id), item_id TEXT UNIQUE);
CREATE TABLE create_reconciliations (
    old_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES client_outbox(operation_id),
    decision TEXT NOT NULL CHECK(decision IN ('applied', 'dismissed')),
    new_operation_id TEXT UNIQUE REFERENCES client_outbox(operation_id),
    CHECK ((decision='applied' AND new_operation_id IS NOT NULL) OR (decision='dismissed' AND new_operation_id IS NULL))
);
PRAGMA user_version=12;
