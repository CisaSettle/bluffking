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

-- The scratch room mirror as it was before 0027.
CREATE UNIQUE INDEX IF NOT EXISTS game_rounds_scratch_identity ON public.game_rounds (((detail->>'lifetime_id')::uuid),((detail->>'revision')::bigint)) WHERE game=16;

CREATE OR REPLACE FUNCTION public.record_scratch_room_result() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE human_players uuid[];
BEGIN
 human_players:=array_remove(ARRAY[(NEW.state->>'shop')::uuid,(NEW.state->>'customer')::uuid],NULL);
 IF cardinality(human_players)>0 AND COALESCE((NEW.state->>'settled')::boolean,FALSE)
    AND jsonb_array_length(NEW.state->'tickets')>0
    AND (NOT COALESCE((OLD.state->>'settled')::boolean,FALSE)
         OR NEW.state->>'sold' IS DISTINCT FROM OLD.state->>'sold') THEN
   INSERT INTO game_rounds(game,id,room_code,ended_at,detail)
   SELECT 16,md5(id::text || ':' || (NEW.state->>'revision'))::uuid,NEW.code,clock_timestamp(),
     jsonb_build_object('lifetime_id',id,'revision',(NEW.state->>'revision')::bigint,
       'tickets',jsonb_array_length(NEW.state->'tickets'),'players',human_players,
       'has_bots',COALESCE((NEW.state->>'bot')::boolean,false))
   FROM game_room_lifetimes WHERE game='scratch' AND code=NEW.code AND ended_at IS NULL
   ON CONFLICT DO NOTHING;
 END IF;
 RETURN NEW;
END $$;

DROP TRIGGER IF EXISTS game_room_result ON public.game_room_checkpoints;
CREATE TRIGGER game_room_result BEFORE UPDATE OF state ON public.game_room_checkpoints FOR EACH ROW WHEN (NEW.game='scratch') EXECUTE FUNCTION public.record_scratch_room_result();

-- Allow a later restart to reapply this explicitly rolled-back increment.
DELETE FROM public.schema_upgrades WHERE name = '0027_lottery_center.up.sql';
