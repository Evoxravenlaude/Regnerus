-- Phase 1 schema. Table names are unqualified: the connection's search_path
-- points at the bot's own schema (DB_SCHEMA, default "regnerus"), which keeps
-- these tables out of the schemas Supabase exposes through its Data API.

create table if not exists accounts (
  id          bigint primary key,            -- Telegram user id
  name        text    not null default '',
  is_admin    boolean not null default false,
  paused      boolean not null default false,
  created_at  bigint  not null
);

create table if not exists invite_codes (
  code        text primary key,
  uses_left   integer not null,
  created_by  bigint  not null,
  created_at  bigint  not null
);

-- The permanent id. Wallets come and go under it.
create table if not exists entities (
  id          bigserial primary key,
  label       text    not null,
  status      text    not null default 'tracking',   -- tracking | lost | retired
  copyable    boolean not null default false,
  bait        boolean not null default false,
  created_by  bigint  not null,
  created_at  bigint  not null
);

create table if not exists wallets (
  address         text primary key,
  entity_id       bigint references entities(id),
  state           text    not null,                   -- seen | candidate | linked | rejected
  score           integer not null default 0,
  linked_by       text,                               -- root | proof | score | admin | review
  first_seen      bigint  not null,
  last_signature  text                                -- backfill cursor
);
create index if not exists wallets_entity_idx on wallets (entity_id);

-- One reason a wallet belongs to an entity. Voided, never deleted.
create table if not exists evidence (
  id           bigserial primary key,
  wallet       text    not null,
  entity_id    bigint  not null,
  kind         text    not null,
  family       text    not null,
  points       integer not null,
  signatures   jsonb   not null default '[]',
  details      jsonb   not null default '{}',
  source       text    not null,                      -- script | admin | ai
  voided       boolean not null default false,
  observed_at  bigint  not null
);
create index if not exists evidence_wallet_idx on evidence (wallet);
create index if not exists evidence_entity_idx on evidence (entity_id);

create table if not exists follows (
  account    bigint not null,
  entity_id  bigint not null,
  mode       text   not null,                         -- off | cards | auto
  primary key (account, entity_id)
);
create index if not exists follows_entity_idx on follows (entity_id);

create table if not exists labels (
  address  text primary key,
  class    text not null,
  name     text not null,
  source   text not null
);

create table if not exists trades (
  signature         text    not null,
  wallet            text    not null,
  mint              text    not null,
  entity_id         bigint,                           -- the entity when the trade was seen
  slot              bigint  not null,
  block_time        bigint,
  seen_at           bigint  not null,
  side              text    not null,
  token_amount_raw  text    not null,                 -- u64 as text
  decimals          integer not null,
  token_program     text,
  quote             text    not null,
  quote_amount_raw  bigint  not null,
  venue             text    not null,
  programs          jsonb   not null default '[]',
  fingerprint       jsonb   not null default '{}',
  primary key (signature, wallet, mint)
);
create index if not exists trades_wallet_slot_idx on trades (wallet, slot desc);
create index if not exists trades_mint_slot_idx on trades (mint, slot);

create table if not exists transfers (
  signature     text    not null,
  wallet        text    not null,
  counterparty  text    not null,
  mint          text    not null default '',          -- '' is native SOL
  direction     text    not null,                     -- in | out
  slot          bigint  not null,
  block_time    bigint,
  amount_raw    text    not null,
  decimals      integer not null,
  primary key (signature, wallet, counterparty, mint, direction)
);
create index if not exists transfers_wallet_slot_idx on transfers (wallet, slot desc);
create index if not exists transfers_counterparty_idx on transfers (counterparty);

-- One entity's buy or sell of one token inside the signal window.
create table if not exists signals (
  id                bigserial primary key,
  entity_id         bigint  not null,
  mint              text    not null,
  side              text    not null,
  first_at          bigint  not null,
  last_at           bigint  not null,
  quote             text    not null,
  quote_amount_raw  bigint  not null,
  token_amount_raw  text    not null,
  decimals          integer not null,
  wallets           jsonb   not null default '[]',
  trades            integer not null,
  venue             text    not null
);
create index if not exists signals_open_idx on signals (entity_id, mint, side, last_at desc);

create table if not exists signal_messages (
  signal_id   bigint not null,
  account     bigint not null,
  message_id  bigint not null,
  primary key (signal_id, account)
);

create table if not exists state (
  key    text primary key,
  value  text not null
);

create table if not exists budget_ledger (
  day      text   not null,
  lane     text   not null,
  credits  bigint not null default 0,
  primary key (day, lane)
);
