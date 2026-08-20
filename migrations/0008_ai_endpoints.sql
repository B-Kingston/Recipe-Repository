-- Multi-endpoint AI credentials: OpenAI-spec and Anthropic-spec API endpoints
-- registered with their keys, switchable from Settings. Replaces the single
-- openai-compatible pi_credentials row; Codex keeps its device-flow credential.
CREATE TABLE ai_endpoints (
  id TEXT PRIMARY KEY NOT NULL,
  user_id TEXT NOT NULL DEFAULT '',
  name TEXT NOT NULL,
  spec TEXT NOT NULL,
  base_url TEXT NOT NULL,
  api_key TEXT NOT NULL,
  model TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX ai_endpoints_user ON ai_endpoints(user_id);

-- Move the legacy OpenAI API credential into the registry under a stable
-- user-facing name; rows without a key could never generate, so they drop.
INSERT INTO ai_endpoints (id, user_id, name, spec, base_url, api_key, model, created_at, updated_at)
  SELECT lower(hex(randomblob(16))), user_id, 'OpenAI API', 'openai',
         COALESCE(json_extract(credential_json, '$.baseUrl'), 'https://api.openai.com/v1'),
         json_extract(credential_json, '$.apiKey'), '',
         updated_at, updated_at
  FROM pi_credentials
  WHERE provider = 'openai-compatible'
    AND json_extract(credential_json, '$.apiKey') IS NOT NULL
    AND json_extract(credential_json, '$.apiKey') <> '';
DELETE FROM pi_credentials WHERE provider = 'openai-compatible';
