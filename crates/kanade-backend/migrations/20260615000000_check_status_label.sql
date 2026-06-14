-- Add a human-facing display title to the compliance projection. The
-- check's stable id (`check_name`, e.g. `defender_rtp`) is a slug; the
-- operator SPA's Compliance page and the Client App's Health tab both
-- read better with an operator-authored title ("ウイルス対策のリアルタイム
-- 保護"). Sourced from the check job's `CheckHint.label`; NULL when the
-- operator didn't set one, in which case the UI falls back to the slug.
ALTER TABLE check_status ADD COLUMN label TEXT;
