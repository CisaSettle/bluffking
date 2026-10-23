-- DLT keeps its two back numbers separate from the scalar bonus used by SSQ
-- and TOTO. Existing periods retain their original numbers and bonus. Applied
-- transactionally by db::initialize together with the schema-upgrade record.
ALTER TABLE public.lottery_periods
 ADD COLUMN IF NOT EXISTS bonus_numbers smallint[] NOT NULL DEFAULT '{}'::smallint[];
