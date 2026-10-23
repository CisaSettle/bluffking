-- Additive only: historical rows deliberately keep NULL proofs.
ALTER TABLE public.lottery_periods ADD COLUMN IF NOT EXISTS proof_plan jsonb;
ALTER TABLE public.lottery_periods ADD COLUMN IF NOT EXISTS draw_proof jsonb;
ALTER TABLE public.lottery_tickets ADD COLUMN IF NOT EXISTS proof_plan jsonb;

-- A pending offer is consumed in the same transaction that persists its reveal
-- with the game receipt. Hence there are at most two pending rows per account.
CREATE TABLE IF NOT EXISTS public.draw_commitments (
 id uuid PRIMARY KEY,
 user_id uuid NOT NULL,
 game text NOT NULL CHECK (game IN ('scratch','sicbo')),
 server_seed text NOT NULL CHECK (server_seed ~ '^[0-9a-f]{64}$'),
 created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (user_id,game)
);

-- An old process sharing this DB cannot draw a planned period without evidence.
DO $$ BEGIN
 IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname='lottery_periods_proof_required' AND conrelid='public.lottery_periods'::regclass) THEN
  ALTER TABLE public.lottery_periods ADD CONSTRAINT lottery_periods_proof_required
   CHECK (proof_plan IS NULL OR status='open' OR draw_proof IS NOT NULL);
 END IF;
END $$;
CREATE OR REPLACE FUNCTION public.lottery_proof_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.proof_plan IS NOT NULL AND NEW.proof_plan IS DISTINCT FROM OLD.proof_plan THEN
  RAISE EXCEPTION 'published lottery plan is immutable';
 END IF;
 IF OLD.status <> 'open' AND ROW(NEW.numbers,NEW.bonus,NEW.bonus_numbers,NEW.draw_proof) IS DISTINCT FROM ROW(OLD.numbers,OLD.bonus,OLD.bonus_numbers,OLD.draw_proof) THEN
  RAISE EXCEPTION 'completed lottery draw is immutable';
 END IF;
 RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS lottery_proof_immutable ON public.lottery_periods;
CREATE TRIGGER lottery_proof_immutable BEFORE UPDATE ON public.lottery_periods
 FOR EACH ROW EXECUTE FUNCTION public.lottery_proof_immutable();
CREATE OR REPLACE FUNCTION public.lottery_ticket_plan_required() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE expected jsonb;
BEGIN
 IF NEW.game <> 'scratch' THEN
  SELECT proof_plan INTO expected FROM public.lottery_periods WHERE game=NEW.game AND period=NEW.period;
  IF expected IS NOT NULL AND NEW.proof_plan IS DISTINCT FROM expected THEN
   RAISE EXCEPTION 'ticket must bind the published lottery plan';
  END IF;
 END IF;
 RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS lottery_ticket_plan_required ON public.lottery_tickets;
CREATE TRIGGER lottery_ticket_plan_required BEFORE INSERT ON public.lottery_tickets
 FOR EACH ROW EXECUTE FUNCTION public.lottery_ticket_plan_required();
