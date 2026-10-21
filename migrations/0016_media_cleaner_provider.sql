-- The video cleaner runs through any AI provider instead of a dedicated
-- Vercel AI Gateway credential. A saved gateway key becomes an ordinary
-- OpenAI-compatible endpoint, keeps the cleaner's former default model, and
-- stays the cleaner's provider; without one the cleaner follows the active
-- AI provider.
INSERT INTO ai_endpoints (id, user_id, name, spec, base_url, api_key, model, created_at, updated_at)
  SELECT lower(hex(randomblob(16))), user_id, 'Vercel AI Gateway', 'openai-compatible',
         COALESCE(NULLIF(json_extract(credential_json, '$.baseUrl'), ''), 'https://ai-gateway.vercel.sh/v1'),
         json_extract(credential_json, '$.apiKey'), 'poolside/laguna-s-2.1-free',
         updated_at, updated_at
  FROM pi_credentials
  WHERE provider = 'vercel-ai-gateway'
    AND COALESCE(json_extract(credential_json, '$.apiKey'), '') <> '';

INSERT OR REPLACE INTO app_settings (key, value, updated_at)
  SELECT 'media_cleaner_provider', id, updated_at
  FROM ai_endpoints
  WHERE name = 'Vercel AI Gateway' AND spec = 'openai-compatible'
  ORDER BY created_at
  LIMIT 1;

DELETE FROM pi_credentials WHERE provider = 'vercel-ai-gateway';
