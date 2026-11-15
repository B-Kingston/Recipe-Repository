-- Groups: a household of accounts that share the recipes they choose and,
-- per member, their AI logins. An account is in at most one group
-- (group_members.user_id is the key), and a group lives only while it has
-- two or more members: the first accepted invite creates it, and the last
-- but one member to leave dissolves it.
CREATE TABLE account_groups (
  id TEXT PRIMARY KEY NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE group_members (
  user_id TEXT PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  group_id TEXT NOT NULL REFERENCES account_groups(id) ON DELETE CASCADE,
  joined_at TEXT NOT NULL
);
CREATE INDEX group_members_group ON group_members(group_id);

-- Email invites. The link carries a random token; the table keeps only its
-- SHA-256, and the address only masked (for the inviter's pending list) and
-- hashed (so inviting the same address again replaces the earlier invite).
CREATE TABLE group_invites (
  id TEXT PRIMARY KEY NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  inviter_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  email_hash TEXT NOT NULL,
  email_masked TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);
CREATE INDEX group_invites_inviter ON group_invites(inviter_id, created_at);

-- One row per invite sent, kept for a day: the per-account daily invite
-- limit counts these, so cancelling an invite does not free a send.
CREATE TABLE group_invite_sends (
  id TEXT PRIMARY KEY NOT NULL,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  sent_at TEXT NOT NULL
);
CREATE INDEX group_invite_sends_user ON group_invite_sends(user_id, sent_at);

-- AI-login grants: granter lets grantee generate with the granter's AI
-- setup. Both must be members of group_id; leaving the group removes the
-- leaver's grants both ways, and dissolving it removes them all.
CREATE TABLE ai_grants (
  group_id TEXT NOT NULL REFERENCES account_groups(id) ON DELETE CASCADE,
  granter_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  grantee_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  PRIMARY KEY (granter_id, grantee_id)
);
CREATE INDEX ai_grants_grantee ON ai_grants(grantee_id);
CREATE INDEX ai_grants_group ON ai_grants(group_id);

-- Shared with the owner's group (read-only for the other members). Toggling
-- it is not an edit: it keeps updated_at and records no version.
ALTER TABLE recipes ADD COLUMN shared INTEGER NOT NULL DEFAULT 0;

-- Upgrade: until now every account shared the oldest account's AI logins
-- unless it chose its own (app_settings.ai_logins = 'own'). An instance with
-- two or more accounts becomes one group holding them all, and the oldest
-- account grants its AI logins to every other account that did not choose
-- its own, so existing households keep working. A single account gets no
-- group. No recipe is shared.
INSERT INTO account_groups (id, created_at)
SELECT lower(substr(h, 1, 8) || '-' || substr(h, 9, 4) || '-4' || substr(h, 14, 3) || '-'
             || substr('89ab', 1 + (abs(random()) % 4), 1) || substr(h, 18, 3) || '-'
             || substr(h, 21, 12)),
       strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now')
FROM (SELECT hex(randomblob(16)) AS h)
WHERE (SELECT COUNT(*) FROM users) >= 2;

INSERT INTO group_members (user_id, group_id, joined_at)
SELECT u.id, g.id, strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now')
FROM users u, account_groups g;

INSERT INTO ai_grants (group_id, granter_id, grantee_id, created_at)
SELECT g.id, oldest.id, u.id, strftime('%Y-%m-%dT%H:%M:%S+00:00', 'now')
FROM account_groups g,
     (SELECT id FROM users ORDER BY created_at, id LIMIT 1) AS oldest,
     users u
WHERE u.id <> oldest.id
  AND NOT EXISTS (
    SELECT 1 FROM app_settings s
    WHERE s.user_id = u.id AND s.key = 'ai_logins' AND s.value = 'own'
  );

-- The grants replace the old per-account choice.
DELETE FROM app_settings WHERE key = 'ai_logins';
