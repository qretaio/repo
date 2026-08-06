-- Symbol store schema (schema_version 2).
-- Embedded into the binary via include_str!; applied with execute_batch.

CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    file_path TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    signature TEXT,
    lang TEXT NOT NULL,
    parent TEXT
);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_path);

CREATE TABLE IF NOT EXISTS imports (
    id INTEGER PRIMARY KEY,
    source_file TEXT NOT NULL,
    target TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_imports_target ON imports(target);
CREATE INDEX IF NOT EXISTS idx_imports_source ON imports(source_file);

CREATE TABLE IF NOT EXISTS tracked_files (
    path TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
