# Regnerus

*Regnerus* is how Saxo Grammaticus, writing the history of the Danes in Latin around 1200, spelled the name of Ragnar Lothbrok.

A Solana watcher and copy trader that follows traders as **entities**, not addresses. An entity has a permanent id (`E#12`). Wallets attach to it and detach from it as evidence comes in, so a trader who rotates wallets keeps one history, one vote in a consensus and one copy setting. This is the Kizzy idea (follow the user id, not the username) applied to wallets.

It is one Rust process with a Postgres database and a Telegram bot in front.

## What it does

**Watches.** One WebSocket to your RPC, one `logsSubscribe` per watched address. Every transaction of a watched wallet is fetched, read as a trade or as transfers, stored, and (for linked wallets) turned into a card. A backfill pass from a stored cursor catches whatever the socket missed.

**Works out whose wallet it is.** This is the trail engine and the clusters. Each finding is a row of evidence with points; a wallet's score is the sum, capped per family.

| Signal | Family | Points |
| --- | --- | --- |
| Paid into the same exchange deposit address as a linked wallet | proof | links outright |
| Received tokens a linked wallet bought, and sold them | funding | 60 |
| Its first SOL came from a linked wallet | funding | 45 |
| It gave a linked wallet its first SOL (a parent in sight) | funding | 45 |
| Exchange withdrawal matching an entity's deposit: one tight fit / one loose fit or a few / many | funding | 40 / 20 / 5 |
| Proceeds go to the same plain wallet | funding | 35 |
| Bought a thinly bought token within 3 slots of the entity / within 60 s | co-buy | 12 / 6 each, cap 36 |
| Builds transactions the same way (app, compute price and limit, tip) | fingerprint | up to 15 |
| Funded from the same exchange in the same amount band | habit | 10 |
| An old wallet went quiet as this one started | timing | 8 |
| Its own deposit address at an exchange where the entity has another | negative | −10 |
| The reviewer's verdict | review | +15, 0 or −25 |

A wallet is **seen** below 40, a **candidate** from 40 (watched, alert only, with Confirm / Not them buttons), and **linked** from 70 *with at least one funding signal*, or by proof, or by an admin. Behaviour alone tops out at 69: it can never link a wallet. An admin's "not them" holds against everything except an admin.

The jobs behind those signals:

- **Trace back**: who first funded a wallet, followed upward to an exchange, a service, a known entity or the hop limit.
- **Exit watch**: where a watched wallet's money goes. A destination that is swept to a labelled exchange hot wallet and has few senders is a personal **deposit address**: everyone who paid into it is linked by proof, and the address becomes a **tripwire**. From then on the stream watches it, and any new wallet that pays in is linked and announced. That is the new-wallet alert.
- **Grandfunder walk** (`/trail`): your Levr method. Inside a time range, total the inflow and outflow of every counterparty above a volume filter, follow the largest flows hop by hop, stop at an exchange. Deposit addresses the target paid into itself become tripwires. Ones further along the trail are leads: their senders get evidence, not a link.
- **CEX hop match**: an entity's deposit to an exchange, paired with a withdrawal of about the same amount (less the fee) to a fresh wallet inside the window.
- **Co-buy scan**: the transactions around each entity buy, read for other wallets buying the same token in the same moment. A crowd in the window says nothing; a wallet that buys beside three different entities is a sniper and is ignored.

**Counts consensus by entity.** Five burners of one trader are one vote. Entities that buy together most of the time form a bloc and count once. A buy made in a crowd does not count. Each user sets how many entities, inside what window, among the ones they follow.

**Maps a token's holders** (`/map`): the largest holders, with pools set aside, grouped by shared funder, direct transfers and shared entity.

**Copies, on paper first.** Every entity buy is copied by the bot itself on paper (a "shadow" copy) at the price it could have got when it saw the buy. An entity becomes **copyable** only when those shadow copies pass: 10 or more closed, a positive result, a median hold of 90 seconds or more, under 20% of sells being tokens it never bought, and no bait flag. Followers on `auto` copy a copyable entity's buys with their own size, caps and exit rules; its sells are mirrored by the same fraction. A wallet tied to an entity that sells minutes after the entity bought flags the entity as **bait** and stops auto-copy.

**Trades live only when told twice.** Real orders need `EXECUTION_LIVE_MODE=true` *and* `EXECUTION_LIVE_ACK=I_UNDERSTAND_THIS_MOVES_REAL_FUNDS` on the server, and `/live on` plus a trading wallet for the user. Until then everything is paper. A live order is a Jupiter Swap V2 `/order` transaction that the bot vets (one signer, ours; allowlisted programs; no approvals or authority changes; SOL out bounded by the order), signs and hands back through `/execute`.

**Asks a model only about the grey band.** A candidate scoring 50 to 69 is sent, as a case file of addresses, numbers and fixed categories, to a reviewer (Claude over the Messages API). Its verdict is one more row of evidence. It cannot link a wallet with no funding signal, a link it carried sends cards only, and every review is stored and graded against proof later. With no key set, the bot runs on its heuristics alone.

**Grades itself.** Each candidate's score is stored when it is first proposed. When proof or an admin settles it, `/weights` shows how often each score band and each signal was right, and proposes measured points. An admin applies them.

## Run it

You need a Telegram bot token (from @BotFather), your Telegram user id, and a Solana RPC endpoint that also serves WebSocket.

```sh
cp .env.example .env      # fill in the three required values
cargo run --release
```

Without `DATABASE_URL` everything is kept in memory and lost on restart: fine for a first look, not for anything you want to keep. With it, the bot creates its own schema (`regnerus`) and applies `migrations/` at startup. On Supabase use the direct or session-pooler connection string (port 5432); the transaction pooler (6543) is refused at startup, because the bot picks its schema once per connection.

On Railway: push the repository and set the variables from `.env.example`. `railway.json` builds the `Dockerfile` and restarts the service whenever it exits. Run one instance: Telegram allows a single long-poll reader per bot token, and the executor lease refuses a second sender.

### First things to do

1. **Label the exchanges your targets use.** The deposit test, the tripwires and the hop match all hinge on knowing which addresses are exchange hot wallets, and none are built in: I could not verify a list from here, and a wrong one would merge strangers. Add them with `/label ADDRESS cex_hot Binance` after checking each on an explorer, or keep them in `labels.json` (`[{"address": "...", "class": "cex_hot", "name": "Binance"}]`). Hot wallets of one exchange share its first word: `Binance 2` and `Binance 3` are both Binance. The eight Jito tip accounts are built in.
2. **Check the trade reader on trades you understand**: `cargo run --release -- classify SIGNATURE WALLET`. The test fixtures were carried over from Yeks and are hand-written, not captured from mainnet. `capture SIGNATURE NAME` saves a real transaction as a fixture.
3. **Track a few wallets and leave it a day.** `/health` and `/budget` then show the delay from block to card and the credits spent, and `/paper` starts to fill.

## Commands

| Command | What it does |
| --- | --- |
| a bare address, or `/track ADDRESS label` | Track a wallet as a new entity; its funder is looked up |
| `/entities`, `/entity ID`, `/wallet ADDRESS` | What is tracked; one entity with its wallets and deposit addresses; a wallet's case file |
| `/candidates` | Wallets waiting for a decision |
| `/trail ADDRESS [days] [min SOL]` | Grandfunder walk |
| `/map MINT` | Holder map of a token |
| `/follow ID off`, `cards` or `auto [SOL]` | Cards only, or copy its buys, optionally at a size of its own |
| `/settings`, `/set KEY VALUE`, `/live on` or `off` | Size, take profit, stop loss, max hold, caps, slippage, consensus; paper or live |
| `/positions`, `/close ID`, `/pnl`, `/paper` | Open copies, closing one, results, and paper results by entity |
| `/wallets`, `/newwallet`, `/usewallet NAME` | Your trading wallet |
| `/weights` | The calibration table |
| `/rename`, `/retire`, `/revive`, `/pause`, `/resume`, `/health`, `/budget` | Housekeeping |
| Admin: `/link`, `/unlink`, `/confirm`, `/merge FROM INTO`, `/split ADDRESS`, `/review ADDRESS` | Corrections to who is who |
| Admin: `/label`, `/unlabel`, `/labels`, `/copyable ID on`, `off` or `auto`, `/weights apply` | Labels, the copyable flag, measured points |
| Admin: `/kill`, `/unkill`, `/code uses`, `/users` | Stop all new copies; access |

Entities and evidence are one shared graph for everyone in the bot. Follows, sizes, modes and trading wallets belong to each user. Private chats only; admins come from `ADMIN_IDS`, everyone else needs an invite code.

## Keys

A trading wallet's key is held by a secrets provider and named by a reference. The database stores the reference and the address, never the key, and the bot never shows a key in chat.

- `SECRETS_PROVIDER=doppler` (recommended): `/newwallet` makes a key and stores it in Doppler when `DOPPLER_WRITE_TOKEN` is set; `/usewallet NAME` points at a secret you stored yourself.
- `SECRETS_PROVIDER=env`: `/usewallet NAME` reads the environment variable `NAME`. `/newwallet` is refused, because a key kept only in the process would be lost with its funds at the next restart.

Fund a trading wallet only with what you are willing to trade. `SOL_RESERVE` stays in it on buys for fees.

## How it is laid out

```
src/
  main.rs, runtime.rs    startup, the long-running tasks, shutdown
  app.rs                 the shared state
  pipeline.rs            signature -> transaction -> trade/transfers -> store -> card; backfill
  chain/solana/          tx (trade, transfer and fingerprint reader), rpc, query, stream, labels
  identity/              score (the arithmetic), behaviour, and the resolver that attaches wallets
  trail/                 the job queue, sampling, back (trace back, hop match), exit, walk
  cluster/               cobuy, bubble
  consensus.rs           blocs and entity votes
  trade/                 gate (pure decisions), engine, sign, venue (Jupiter), keys
  review/                reviewer (trait and client), casefile, calibration
  bot/                   telegram, cards, commands, more
  store/                 the Store trait, Postgres, and the in-memory store
  obs/                   the RPC budget (three lanes) and health
migrations/              SQL, applied in order under an advisory lock
tests/                   137 tests
```

The chain, the store, Telegram, the swap venue, the key provider and the reviewer each sit behind a trait, so everything above them is tested whole against fakes.

## Tests

```sh
cargo test
TEST_DATABASE_URL=postgres://... cargo test     # also runs the store suite against Postgres
cargo clippy --all-targets -- -D warnings
```

`classify`, `pipeline`, `bot`, `stream`, `budget`, `store` cover the watcher. `identity` covers the scoring rules, states, notices, merge and split. `trail` runs trace back, exit watch, the deposit test, tripwires, hop matching and the walk against a scripted chain. `cluster` covers co-buys, snipers, crowds, blocs, consensus and the holder map. `trader` covers transaction bytes (parse, vet, sign, verified with ed25519), the gate, the screen, and the copy path on paper and live against a fake venue. `review` covers the reviewer and calibration. `live` runs the real HTTP and WebSocket clients (RPC, stream, Jupiter, the Messages API, Doppler) against local servers.

## What has not been checked, and known limits

- **Nothing here has touched mainnet, Telegram, Jupiter, the Messages API or Doppler.** The workspace it was built in can reach none of them. Every client is tested against a local server that answers as the documentation says the real one does. The first real run is yours: watch the log, then `/health`.
- **Live trading is untested with real funds.** Do the paper week first, then start with a size you can lose. Read `src/trade/sign.rs` and `src/trade/engine.rs` before turning it on: that is the whole money path.
- **The hop match counts fits among wallets the bot has traced**, not among all of an exchange's withdrawals: reading every withdrawal of a large exchange for six hours costs more credits than a standard RPC plan has. So a "single fit" is single among known wallets. A tight fit also needs a non-round amount inside an hour.
- **A wallet with more than 3,000 transactions has no first funder** as far as the bot is concerned: paging further back costs too much. Trace back works on fresh wallets, which is where it matters.
- **"Thinly bought" is measured in transactions** mentioning the token in the minute around the buy, not in distinct buyers, so that a crowd is recognised from one listing without reading it.
- **The co-buy scan finds trades that name the token's mint.** pump.fun and PumpSwap trades do. Some pools (Raydium's older AMM, for one) name only token accounts, and co-buyers there are missed.
- **Incoming token transfers can be missed live**: `logsSubscribe` fires for transactions that mention the wallet's own address, and a token sent to its token account by someone else does not always.
- **Calibration measures each signal on its own**, as log-odds of being right. It is not a joint fit.
- **Position sizes are in SOL**, and a trade is one token against SOL, wSOL, USDC or USDT.
- **Not built**: Helius identity and funded-by lookups, the Bubblemaps cross-check, a drawn holder map, the `/build` landing path, the EVM adapter and the FOMO lens. The design doc lists them as optional or later.
