ALTER TABLE thread_dynamic_tools
ADD COLUMN persist_on_resume INTEGER NOT NULL DEFAULT 1;

ALTER TABLE thread_dynamic_tools
ADD COLUMN capability_json TEXT;

ALTER TABLE thread_dynamic_tools
ADD COLUMN namespace_description TEXT;
