-- Settings belong to an account, like the credentials and endpoints they
-- point at: each user keeps their own AI provider, models, reasoning effort,
-- video cleaner, and step-moments switch. Existing values go to the oldest
-- account, or stay unowned ('') until setup claims them.
CREATE TABLE app_settings_new (
  user_id TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (user_id, key)
);
INSERT INTO app_settings_new (user_id, key, value, updated_at)
  SELECT COALESCE((SELECT id FROM users ORDER BY created_at LIMIT 1), ''),
         key, value, updated_at
  FROM app_settings;
DROP TABLE app_settings;
ALTER TABLE app_settings_new RENAME TO app_settings;
