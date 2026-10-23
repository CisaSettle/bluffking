-- Manual rollback only: removes the lottery center tables and narrows the
-- wallet whitelist back to before LOTTERY (23). Refuses while any lottery
-- wallet exists; never deletes account data.
DO $$
DECLARE
 def text;
BEGIN
 IF EXISTS (SELECT 1 FROM public.wallets WHERE game = 23)
  OR EXISTS (SELECT 1 FROM public.wallet_ledger WHERE game = 23)
  OR EXISTS (SELECT 1 FROM public.lottery_tickets)
  OR EXISTS (SELECT 1 FROM public.lottery_winners)
  OR EXISTS (SELECT 1 FROM public.lottery_day_user_stats)
  OR EXISTS (SELECT 1 FROM public.lottery_periods WHERE tickets > 0 OR sales > 0 OR paid > 0) THEN
  RAISE EXCEPTION '0027_lottery_center down: lottery business rows exist; refusing (data preserved)';
 END IF;
 SELECT pg_get_constraintdef(c.oid) INTO def FROM pg_constraint c
  WHERE c.conrelid = 'public.wallets'::regclass AND c.conname = 'wallets_game_check';
 IF def IS NOT NULL AND strpos(def, '20, 21, 22, 23]') > 0 THEN
  EXECUTE format('ALTER TABLE public.wallets DROP CONSTRAINT wallets_game_check, ADD CONSTRAINT wallets_game_check %s',
   replace(def, '20, 21, 22, 23]', '20, 21, 22]'));
 END IF;
END $$;

DROP INDEX IF EXISTS public.wallet_ledger_lottery_refill;
DROP TABLE IF EXISTS public.lottery_day_user_stats;
DROP TABLE IF EXISTS public.lottery_presence;
DROP TABLE IF EXISTS public.lottery_winners;
DROP TABLE IF EXISTS public.lottery_tickets;
DROP TABLE IF EXISTS public.lottery_periods;

-- Allow a later restart to reapply this explicitly rolled-back increment.
DELETE FROM public.schema_upgrades WHERE name = '0027_lottery_center.up.sql';
