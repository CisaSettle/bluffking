-- 彩票中心 (lottery center): one server-authoritative hall per lottery game
-- (ssq / toto / happy8 / fucai3d / scratch) replaces the two-seat scratch
-- rooms and the tab-local number games. Additive: new tables only, each
-- `IF NOT EXISTS`; the practice wallet whitelist gains game 23 (LOTTERY).
-- No physical foreign keys (account purge removes rows explicitly).
--
-- The scratch room trigger that mirrored settled rooms into game_rounds
-- (game=16) goes with the room model; existing game_rounds rows stay for the
-- daily report's history. Old scratch checkpoint rows are NOT deleted here:
-- the lottery retention sweep retires them with the old policy (closed > 1 d,
-- any > 7 d) and account purge still passes through them.

CREATE TABLE IF NOT EXISTS public.lottery_periods (
 game text NOT NULL,
 period bigint NOT NULL,
 opens_at timestamptz NOT NULL,
 draw_at timestamptz NOT NULL,
 status text NOT NULL DEFAULT 'open' CHECK (status IN ('open','drawn','settled')),
 numbers smallint[],
 bonus smallint,
 drawn_at timestamptz,
 settled_at timestamptz,
 tickets integer NOT NULL DEFAULT 0,
 stakes integer NOT NULL DEFAULT 0,
 players integer NOT NULL DEFAULT 0,
 sales bigint NOT NULL DEFAULT 0,
 paid bigint NOT NULL DEFAULT 0,
 winners integer NOT NULL DEFAULT 0,
 PRIMARY KEY (game, period)
);
-- Latest draw / recent draws per game.
CREATE INDEX IF NOT EXISTS lottery_periods_drawn ON public.lottery_periods (game, drawn_at DESC)
 WHERE status <> 'open';
-- Draw-loop work: open periods due for a draw, drawn periods due for settlement.
CREATE INDEX IF NOT EXISTS lottery_periods_work ON public.lottery_periods (game, period)
 WHERE status <> 'settled';
-- Retention of settled periods (30 days).
CREATE INDEX IF NOT EXISTS lottery_periods_retire ON public.lottery_periods (settled_at)
 WHERE status = 'settled';

CREATE TABLE IF NOT EXISTS public.lottery_tickets (
 id uuid PRIMARY KEY,
 purchase_id uuid NOT NULL,
 game text NOT NULL,
 period bigint NOT NULL,                 -- scratch: 0
 user_id uuid NOT NULL,
 bought_at timestamptz NOT NULL DEFAULT now(),
 entry jsonb NOT NULL,
 stakes integer NOT NULL,
 multiplier smallint NOT NULL DEFAULT 1,
 cost bigint NOT NULL,
 revealed boolean[],                     -- scratch only
 prize bigint,
 tier smallint,
 settled_at timestamptz
);
-- Viewer's tickets of a period; per-period caps.
CREATE INDEX IF NOT EXISTS lottery_tickets_period_user ON public.lottery_tickets (game, period, user_id);
-- Settlement work per period, one user at a time; unfinished scratch cards.
CREATE INDEX IF NOT EXISTS lottery_tickets_unsettled ON public.lottery_tickets (game, period, user_id, id)
 WHERE settled_at IS NULL;
-- Viewer's ticket history per game (newest first) and account purge.
CREATE INDEX IF NOT EXISTS lottery_tickets_user_recent ON public.lottery_tickets (user_id, game, bought_at DESC);
-- Hall feed: the newest purchases of a game.
CREATE INDEX IF NOT EXISTS lottery_tickets_feed ON public.lottery_tickets (game, bought_at DESC);
-- Replayed purchases return the stored tickets.
CREATE INDEX IF NOT EXISTS lottery_tickets_purchase ON public.lottery_tickets (purchase_id);
-- Retention of settled tickets (30 days).
CREATE INDEX IF NOT EXISTS lottery_tickets_retire ON public.lottery_tickets (settled_at)
 WHERE settled_at IS NOT NULL;

-- One row per winning user per period; `id` is the user's draw credit id so a
-- re-run settlement cannot duplicate it. `user_id` is nulled by account purge.
CREATE TABLE IF NOT EXISTS public.lottery_winners (
 id uuid NOT NULL,
 game text NOT NULL,
 period bigint NOT NULL,
 user_id uuid,
 name text NOT NULL,
 tier smallint NOT NULL,
 prize bigint NOT NULL,
 PRIMARY KEY (game, period, id)
);
CREATE INDEX IF NOT EXISTS lottery_winners_board ON public.lottery_winners (game, period, prize DESC);
CREATE INDEX IF NOT EXISTS lottery_winners_user ON public.lottery_winners (user_id);

-- Who is in the hall: upserted by the view at most once per 10 s per user.
CREATE TABLE IF NOT EXISTS public.lottery_presence (
 game text NOT NULL,
 user_id uuid NOT NULL,
 name text NOT NULL,
 seen_at timestamptz NOT NULL,
 PRIMARY KEY (game, user_id)
);
CREATE INDEX IF NOT EXISTS lottery_presence_user ON public.lottery_presence (user_id);
CREATE INDEX IF NOT EXISTS lottery_presence_recent ON public.lottery_presence (game, seen_at DESC);

-- Daily report facts, one row per (day, game, user), written only by the
-- statistics writer after a purchase / settlement committed (single writer,
-- no locks). tickets/stakes/sales count by purchase day, paid by settlement day.
CREATE TABLE IF NOT EXISTS public.lottery_day_user_stats (
 day date NOT NULL,
 game text NOT NULL,
 user_id uuid NOT NULL,
 tickets bigint NOT NULL DEFAULT 0,
 stakes bigint NOT NULL DEFAULT 0,
 sales bigint NOT NULL DEFAULT 0,
 paid bigint NOT NULL DEFAULT 0,
 PRIMARY KEY (day, game, user_id)
);
CREATE INDEX IF NOT EXISTS lottery_day_user_stats_user ON public.lottery_day_user_stats (user_id);

-- Refill replay lookup (`POST /api/lottery/refill` is idempotent on its
-- request_id, kept in the ledger row's details) without walking the user's
-- whole lottery ledger.
CREATE INDEX IF NOT EXISTS wallet_ledger_lottery_refill ON public.wallet_ledger (user_id, (details->>'request_id'))
 WHERE game = 23 AND kind = 'refill';

-- The practice wallet whitelist gains LOTTERY (23). Same shape as 0012: the
-- constraint is rebuilt only while it still lacks the game, so later startups
-- and a fresh 0001_current.sql install (already widened) are no-ops.
DO $$
DECLARE
 def text;
BEGIN
 SELECT pg_get_constraintdef(c.oid) INTO def FROM pg_constraint c
  WHERE c.conrelid = 'public.wallets'::regclass AND c.conname = 'wallets_game_check';
 IF def IS NULL THEN
  RAISE EXCEPTION '0027_lottery_center: constraint wallets.wallets_game_check missing; data preserved';
 END IF;
 IF strpos(def, '20, 21, 22, 23]') = 0 THEN
  IF strpos(def, '20, 21, 22]') = 0 THEN
   RAISE EXCEPTION '0027_lottery_center: unexpected wallets.wallets_game_check: %; data preserved', def;
  END IF;
  EXECUTE format('ALTER TABLE public.wallets DROP CONSTRAINT wallets_game_check, ADD CONSTRAINT wallets_game_check %s',
   replace(def, '20, 21, 22]', '20, 21, 22, 23]'));
 END IF;
END $$;

-- The scratch room → game_rounds mirror ends with the room model.
DROP TRIGGER IF EXISTS game_room_result ON public.game_room_checkpoints;
DROP FUNCTION IF EXISTS public.record_scratch_room_result();
DROP INDEX IF EXISTS public.game_rounds_scratch_identity;

CREATE INDEX IF NOT EXISTS lottery_tickets_scratch_expiry ON public.lottery_tickets (bought_at, id) WHERE game='scratch' AND settled_at IS NULL;
CREATE INDEX IF NOT EXISTS lottery_presence_retire ON public.lottery_presence (seen_at);
