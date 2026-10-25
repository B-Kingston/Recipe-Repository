-- When a provider last refused a login: an OAuth refresh or bearer token that
-- the subscription rejected, or an API key the endpoint rejected. NULL means
-- the login worked (or has not failed) since it was saved, refreshed, or
-- checked. Settings shows it so an expired login is found before a generation.
ALTER TABLE pi_credentials ADD COLUMN auth_failed_at TEXT;
ALTER TABLE ai_endpoints ADD COLUMN auth_failed_at TEXT;
