CREATE TABLE users (
  id TEXT PRIMARY KEY NOT NULL,
  username TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,
  created_at TEXT NOT NULL
);

ALTER TABLE recipes ADD COLUMN user_id TEXT NOT NULL DEFAULT '';
CREATE INDEX recipes_user_updated ON recipes(user_id, updated_at);

ALTER TABLE ai_drafts ADD COLUMN user_id TEXT NOT NULL DEFAULT '';

-- pi_credentials: single global row -> one row per (user, provider)
CREATE TABLE pi_credentials_new (
  user_id TEXT NOT NULL,
  provider TEXT NOT NULL,
  credential_json TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (user_id, provider)
);
INSERT INTO pi_credentials_new (user_id, provider, credential_json, updated_at)
  SELECT '', provider, credential_json, updated_at FROM pi_credentials;
DROP TABLE pi_credentials;
ALTER TABLE pi_credentials_new RENAME TO pi_credentials;
