-- An optional recovery address per account. users.email holds only a
-- verified address (both columns are set together, by following a
-- verification link or by an invite sent to that address); an address
-- waiting for verification lives only in its email_tokens row. No two
-- accounts share a verified address, letter case ignored.
ALTER TABLE users ADD COLUMN email TEXT;
ALTER TABLE users ADD COLUMN email_verified_at TEXT;
CREATE UNIQUE INDEX users_verified_email ON users(email COLLATE NOCASE)
  WHERE email_verified_at IS NOT NULL;

-- Emailed links: 'verify' (proves email for user_id, 24 hours) and 'reset'
-- (sets a new password, 1 hour). Each works once; the table keeps only the
-- token's SHA-256. An account has at most one live token per purpose.
CREATE TABLE email_tokens (
  token_hash TEXT PRIMARY KEY NOT NULL,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  purpose TEXT NOT NULL CHECK (purpose IN ('verify', 'reset')),
  email TEXT,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);
CREATE INDEX email_tokens_user ON email_tokens(user_id, purpose);
