-- Phases 2 to 6: deposit addresses, the forensics queue, keyed documents
-- and positions.

create table if not exists deposit_addrs (
  address     text primary key,
  entity_id   bigint  not null,
  exchange    text    not null,
  status      text    not null,                       -- pending | qualified | rejected
  senders     integer not null default 0,
  first_seen  bigint  not null
);
create index if not exists deposit_addrs_entity_idx on deposit_addrs (entity_id);

create table if not exists trail_jobs (
  id          bigserial primary key,
  kind        text    not null,
  subject     text    not null,
  entity_id   bigint,
  payload     jsonb   not null default '{}',
  status      text    not null default 'queued',      -- queued | running | done | failed
  attempts    integer not null default 0,
  run_after   bigint  not null,
  created_at  bigint  not null
);
-- One live job per kind and subject.
create unique index if not exists trail_jobs_live_idx on trail_jobs (kind, subject) where status in ('queued', 'running');
create index if not exists trail_jobs_due_idx on trail_jobs (status, run_after);

-- Small keyed JSON records: funder cache, co-buy windows, settings,
-- trading-wallet references, reviews, score snapshots, weights.
create table if not exists docs (
  kind        text   not null,
  key         text   not null,
  value       jsonb  not null,
  updated_at  bigint not null,
  primary key (kind, key)
);

create table if not exists positions (
  id              bigserial primary key,
  account         bigint  not null,                   -- 0 is the bot's own shadow copy
  entity_id       bigint  not null,
  signal_id       bigint  not null,
  mint            text    not null,
  decimals        integer not null,
  mode            text    not null,                   -- shadow | paper | live
  status          text    not null,                   -- opening | open | closed | failed | unconfirmed
  entry_lamports  bigint  not null,
  tokens_raw      text    not null,
  tokens_left     text    not null,
  exit_lamports   bigint  not null default 0,
  opened_at       bigint  not null,
  closed_at       bigint,
  exit_reason     text,
  open_signature  text,
  note            text    not null default '',
  unique (account, signal_id, mode)
);
create index if not exists positions_open_idx on positions (status);
create index if not exists positions_account_idx on positions (account, opened_at desc);
create index if not exists positions_entity_idx on positions (entity_id, mode);

create index if not exists signals_last_idx on signals (last_at);
create index if not exists evidence_kind_idx on evidence (kind);
