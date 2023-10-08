CREATE TABLE IF NOT EXISTS server (
  id INTEGER PRIMARY KEY,
  endpoint TEXT NOT NULL UNIQUE,
  timeout INTEGER
);

CREATE TABLE IF NOT EXISTS subscription (
  topic TEXT,
  display_name TEXT,
  muted INTEGER NOT NULL DEFAULT 0,
  server INTEGER REFERENCES server(id),
  archived INTEGER NOT NULL DEFAULT 0,
  reserved INTEGER NOT NULL DEFAULT 0,
  read_until INTEGER NOT NULL DEFAULT 0,
  symbolic_icon TEXT,
  PRIMARY KEY (server, topic)
);

CREATE TABLE IF NOT EXISTS message (
  server INTEGER,
  data TEXT NOT NULL,
  topic TEXT AS (data ->> '$.topic'), -- For the FOREIGN KEY constraint
  FOREIGN KEY (server, topic) REFERENCES subscription(server, topic) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS message_by_time ON message (data ->> '$.time');
-- I can't put a JSON expression inside a UNIQUE constraint,
-- but I can do it on a UNIQUE INDEX
CREATE UNIQUE INDEX IF NOT EXISTS server_and_message_id ON message (server, data ->> '$.id');
