CREATE TABLE pi_credentials (
  provider TEXT PRIMARY KEY NOT NULL,
  credential_json TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
