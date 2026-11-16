-- The backfill's daily request ceiling, counted here rather than in the process: a
-- restart handed out a fresh day's allowance, so a redeploy on 2026-11-16 spent ~750
-- requests against a ceiling of 400. The day is UTC, as `runner` reads it.

ALTER TABLE sync_state
  ADD COLUMN budget_day   date,
  ADD COLUMN budget_spent bigint NOT NULL DEFAULT 0;
