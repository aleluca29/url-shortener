ALTER TABLE urls ADD COLUMN expires_at TEXT;
ALTER TABLE urls ADD COLUMN created_ip TEXT;
ALTER TABLE urls ADD COLUMN created_user_agent TEXT;

ALTER TABLE clicks ADD COLUMN user_agent TEXT;
ALTER TABLE clicks ADD COLUMN referer TEXT;
ALTER TABLE clicks ADD COLUMN country TEXT;
ALTER TABLE clicks ADD COLUMN city TEXT;

CREATE INDEX IF NOT EXISTS idx_clicks_code_at ON clicks(code, at);
CREATE INDEX IF NOT EXISTS idx_clicks_code_ip ON clicks(code, ip);