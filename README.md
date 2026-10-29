# prisma-split

Solana program (Pinocchio) that routes a Raydium LaunchLab coin's trading fees to up to 5 recipients: wallets, or
X / Telegram / GitHub accounts by user id.

Every coin launched through `launch` + `create` gets its own LaunchLab platform whose admin, fee wallet and pool creator
are the program's per-coin inbox PDA `["inbox", mint]`; the fee rate is the one the coin's creator chose. `distribute`
claims the platform fee (bonding curve) and, after migration, the CPMM creator fee, unwraps the SOL and splits it: a
platform share (basis points fixed at launch) and the recipients by their basis points. Wallets and bound accounts are
paid at once; an account `["acct", kind, id]` is bound to a wallet once by the binder key, within a daily cap on
everything that key directs. Dues unpaid for the period go to a sink wallet (`expire`). The same ticker cannot
launch again for 60 minutes. A coin may pair with another token than SOL (USDC, a tokenized stock) once the admin
allowed it (`["quote", mint]`, off by default): its fees then stay in the inbox's token account and are paid in that
token. Paused, the admin retires each coin's platform, refunds every due and closes all accounts.

| tag | instruction |
|---|---|
| 0 | init_config (upgrade authority; starts paused) |
| 1 | set_config |
| 2 | set_coin_mint (once) |
| 3 | launch (author + mint: recipients, ticker lock, the coin's platform) |
| 4 | create (author + mint: the LaunchLab pool, within 120 s of launch) |
| 5 | distribute (anyone) |
| 6 | bind (binder) |
| 7 | claim (anyone; pays the slot owner) |
| 8 | claim_platform (anyone) |
| 9 | expire (anyone; overdue due -> sink) |
| 10 | retire (admin, paused) |
| 11 | refund_coin (admin, paused) |
| 12 | close_ticker (anyone, after the hour) |
| 13 | close_acct (admin, paused, after every coin) |
| 14 | close_config (admin, paused, nothing open) |
| 15 | set_quote (admin: allow / switch a pair, its daily cap) |
| 16 | close_quote (admin, paused, after every coin) |

Build: `cargo build-sbf`. Client: `client/prisma.mjs`. Mainnet-fork suite (surfpool, real LaunchLab and CPMM):
`RPC_URL=<mainnet rpc> bash client/fork.sh`.
