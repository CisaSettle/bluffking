-- Do not discard committed DLT draws on rollback. Retaining the additive
-- column is compatible with the previous backend, so populated deployments
-- can safely roll back code while keeping their accounting evidence.
DO $$
BEGIN
 IF EXISTS (
  SELECT 1 FROM information_schema.columns
  WHERE table_schema='public' AND table_name='lottery_periods' AND column_name='bonus_numbers'
 ) THEN
  IF EXISTS (SELECT 1 FROM public.lottery_periods WHERE cardinality(bonus_numbers)>0) THEN
   RAISE EXCEPTION '0029_daletou: populated back numbers must be preserved; roll back code without dropping the column';
  END IF;
  ALTER TABLE public.lottery_periods DROP COLUMN bonus_numbers;
 END IF;
END $$;
