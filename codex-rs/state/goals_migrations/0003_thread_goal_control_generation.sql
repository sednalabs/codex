ALTER TABLE thread_goals
ADD COLUMN control_generation INTEGER NOT NULL DEFAULT 0 CHECK(control_generation >= 0);
