ALTER TABLE scheduled_embed_sends ADD COLUMN mention_json TEXT NOT NULL DEFAULT '{"roles":[],"everyone":false,"here":false}';
ALTER TABLE scheduled_embed_sends ADD COLUMN mention_outcome TEXT;
