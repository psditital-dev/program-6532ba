//! Prisma fee split. Every coin launched through `launch` gets its own Raydium LaunchLab platform: admin, fee wallet
//! and pool creator are the program's per-coin inbox PDA ["inbox", mint], the fee rate is the one the coin's creator
//! chose. `distribute` (anyone; our server every 2 min) claims the coin's LaunchLab platform fee and, after migration,
//! its CPMM creator fee into the inbox, unwraps the SOL and splits it: the platform share to the config, the rest to
//! up to 5 recipients (wallets, or X / Telegram / GitHub accounts by id). Wallets and bound
//! accounts are paid at once, the rest waits as the slot's due. An account ["acct", kind, id] is bound to a wallet
//! once by our server key after a social sign-in (bounded: a daily cap on what it directs). Dues unpaid for
//! the period (14 days) go to the sink wallet (bought back and burned). See README.md.
#![cfg_attr(not(test), no_std)]

use pinocchio::cpi::{invoke_signed_with_bounds, Seed, Signer};
use pinocchio::error::ProgramError;
use pinocchio::instruction::{InstructionAccount, InstructionView};
use pinocchio::sysvars::{clock::Clock, rent::Rent, Sysvar};
use pinocchio::{AccountView, Address, ProgramResult};
use pinocchio_system::instructions::{Allocate, Assign, Transfer};

#[cfg(not(feature = "no-entrypoint"))]
pinocchio::program_entrypoint!(process_instruction, 48);
pinocchio::no_allocator!();
pinocchio::nostd_panic_handler!();

// Program address.
pub const ID: Address = Address::from_str_const("8gLoG9dDBvpjwACiP7ygfq1fBLjSLV3neHLVgBcZmqGz");
const SYSTEM: Address = pinocchio_system::ID;
const LOADER: Address = Address::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
const SYSVAR: Address = Address::from_str_const("Sysvar1111111111111111111111111111111111111");
const LAUNCHLAB: Address = Address::from_str_const("LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj");
const CPMM: Address = Address::from_str_const("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
const TOKEN: Address = Address::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA: Address = Address::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const WSOL: Address = Address::from_str_const("So11111111111111111111111111111111111111112");
const ZERO: Address = Address::new_from_array([0; 32]);
// Runtime-reserved ids that are never writable yet are neither executable nor sysvar-owned.
const RESERVED: [Address; 5] = [
    Address::from_str_const("StakeConfig11111111111111111111111111111111"),
    Address::from_str_const("LoaderV411111111111111111111111111111111111"),
    Address::from_str_const("ZkTokenProof1111111111111111111111111111111"),
    Address::from_str_const("NativeLoader1111111111111111111111111111111"),
    SYSVAR,
];

const BPS: u64 = 10_000;
const MAX_SLOTS: usize = 5; // TZ §2
const MAX_PLAT_BPS: u16 = 5_000;
const MAX_TIP: u64 = 50_000;
const MAX_KINDS: u8 = 8; // 1 X, 2 Telegram, 3 GitHub, 4 Instagram; 5..8 reserved for the TZ update (YouTube, TikTok, Discord, Twitch)
const FEE_CAP: u64 = 50_000; // 5 %, LaunchLab rates are in 1e-6
const LP_ALL: u64 = 1_000_000;
const DAY: i64 = 86_400;
const LOCK: i64 = 3_600; // the same ticker cannot launch again for 60 min
const WINDOW: i64 = 120; // `create` must follow its `launch` within this; the ticker lock runs from the window's end
const PLATFORM_LEN: usize = 944; // LaunchLab PlatformConfig account
const TOKEN_LEN: usize = 165;
// defaults set by init_config, changed by set_config
const PLAT_BPS: u16 = 1_000; // 10 % of every coin's fee stream (team approval pending)
const BUYBACK_BPS: u16 = 4_000; // of the platform money: $PRISM buyback and burn (Valentin 29.10)
const TEAM_BPS: u16 = 2_000; // team; the rest (40 %) goes to the $PRISM holders wallet
const PERIOD: i64 = 14 * DAY; // dues not taken for 14 days go to the sink wallet (buyback and burn)
const MIN_PERIOD: i64 = 60;
const KIND_MAX: u8 = 3; // X, Telegram, GitHub (Instagram off, Valentin 29.10)
const CAP: u64 = 5_000_000_000; // what our server key can direct a day: 5 SOL
const TIP: u64 = 10_000; // to whoever pays the fee of a distribute / sponsored claim
const MIN_FEE: u64 = 2_500; // 0.25 %
const MAX_FEE: u64 = FEE_CAP;

const CONFIG: u8 = 1;
const COIN: u8 = 2;
const ACCT: u8 = 3;
const TICKER: u8 = 4;
const WALLET: u8 = 0;
const BOUND: u8 = 1; // slot flag: owner came from a server binding, payouts count toward the daily cap

mod c {
    pub const BUMP: usize = 1;
    pub const ADMIN: usize = 2;
    pub const BINDER: usize = 34;
    pub const BUYBACK: usize = 66;
    pub const TEAM: usize = 98; // also the LaunchLab LP NFT wallet when LP_SCALE > 0
    pub const HOLDERS: usize = 130; // $PRISM holders' share
    pub const COIN_MINT: usize = 162; // $PRISM, set once by the admin on coin day (info)
    pub const CP_CONFIG: usize = 194; // Raydium CPMM config every coin migrates to
    pub const PLAT_BPS: usize = 226; // u16
    pub const BUYBACK_BPS: usize = 228; // u16
    pub const TEAM_BPS: usize = 230; // u16
    pub const PAUSED: usize = 232;
    pub const SWEEP: usize = 233; // u8, 1 = dues unpaid for PERIOD go to SINK (`expire`)
    pub const KIND_MAX: usize = 234; // u8, highest account kind a launch may name
    pub const CAP: usize = 235; // u64
    pub const DAY: usize = 243; // i64
    pub const USED: usize = 251; // u64
    pub const TIP: usize = 259; // u64
    pub const OPEN: usize = 267; // u32, coins + tickers + accounts not yet closed
    pub const PERIOD: usize = 271; // i64
    pub const MIN_FEE: usize = 279; // u64
    pub const MAX_FEE: usize = 287; // u64
    pub const LP_SCALE: usize = 295; // u64, LP share (1e6) to the team as a fee key NFT at migration; the rest is burned
    pub const PLAT_DUE: usize = 303; // u64, platform money held in this account
    pub const COINS: usize = 311; // u32, coin records not yet refunded (bindings close only after them)
    pub const SINK: usize = 315; // where expired dues go (default the buyback wallet: bought back and burned)
    pub const LEN: usize = 347;
}
mod k {
    pub const BUMP: usize = 1;
    pub const IBUMP: usize = 2;
    pub const N: usize = 3;
    pub const PLAT_BPS: usize = 4; // u16, snapshot at launch
    pub const MINT: usize = 6;
    pub const AUTHOR: usize = 38; // launched the coin, paid its rent; unbound dues on wind-down
    pub const PLATFORM: usize = 70; // the coin's LaunchLab platform config
    pub const TOTAL: usize = 102; // u64, lifetime booked (stats)
    pub const SYM: usize = 110; // u8 length + 10 bytes: the symbol `create` must use
    pub const TIME: usize = 121; // i64, `launch` time: `create` within WINDOW of it
    pub const SLOTS: usize = 129;
    pub const LEN: usize = SLOTS + super::MAX_SLOTS * super::s::LEN;
}
mod s {
    pub const KIND: usize = 0;
    pub const FLAGS: usize = 1;
    pub const KEY: usize = 2; // wallet address, or the account's numeric id as u64 LE (rest zero)
    pub const BPS: usize = 34; // u16, share of the after-platform rest
    pub const OWNER: usize = 36; // zero until a claim through a bound account
    pub const DUE: usize = 68; // u64
    pub const SINCE: usize = 76; // i64, launch or the last payout / sweep: the unpaid clock
    pub const LEN: usize = 84;
}
mod ac {
    pub const BUMP: usize = 1;
    pub const KIND: usize = 2;
    pub const ID: usize = 3; // u64
    pub const OWNER: usize = 11;
    pub const PAYER: usize = 43; // rent back on wind-down
    pub const LEN: usize = 75;
}
mod t {
    pub const BUMP: usize = 1;
    pub const TIME: usize = 2; // i64, last launch with this ticker
    pub const PAYER: usize = 10;
    pub const LEN: usize = 42;
}

#[repr(u32)]
enum Error {
    Unauthorized = 1,
    Paused,
    NotPaused,
    BadParams,
    BadAccount,
    MathOverflow,
    AlreadyBound,
    NothingToClaim,
    CapReached,
    PayoutBelowRent,
    AccountsOpen,
    NotExpired,
    Expired,
    TickerLocked,
}
fn err(e: Error) -> ProgramError {
    ProgramError::Custom(e as u32)
}
fn need(ok: bool, e: Error) -> ProgramResult {
    if ok {
        Ok(())
    } else {
        Err(err(e))
    }
}
fn rd<const N: usize>(d: &[u8], o: usize) -> Result<[u8; N], ProgramError> {
    let end = o.checked_add(N).ok_or(ProgramError::InvalidAccountData)?;
    d.get(o..end)
        .ok_or(ProgramError::InvalidAccountData)?
        .try_into()
        .map_err(|_| ProgramError::InvalidAccountData)
}
fn byte(d: &[u8], o: usize) -> Result<u8, ProgramError> {
    d.get(o).copied().ok_or(ProgramError::InvalidAccountData)
}
fn key(d: &[u8], o: usize) -> Result<Address, ProgramError> {
    Ok(Address::new_from_array(rd(d, o)?))
}
fn u16_at(d: &[u8], o: usize) -> Result<u16, ProgramError> {
    Ok(u16::from_le_bytes(rd(d, o)?))
}
fn u32_at(d: &[u8], o: usize) -> Result<u32, ProgramError> {
    Ok(u32::from_le_bytes(rd(d, o)?))
}
fn u64_at(d: &[u8], o: usize) -> Result<u64, ProgramError> {
    Ok(u64::from_le_bytes(rd(d, o)?))
}
fn i64_at(d: &[u8], o: usize) -> Result<i64, ProgramError> {
    Ok(i64::from_le_bytes(rd(d, o)?))
}
fn put(d: &mut [u8], o: usize, v: &[u8]) -> ProgramResult {
    let end = o.checked_add(v.len()).ok_or(ProgramError::InvalidAccountData)?;
    d.get_mut(o..end)
        .ok_or(ProgramError::InvalidAccountData)?
        .copy_from_slice(v);
    Ok(())
}
fn add(a: u64, b: u64) -> Result<u64, ProgramError> {
    a.checked_add(b).ok_or(err(Error::MathOverflow))
}
fn sub(a: u64, b: u64) -> Result<u64, ProgramError> {
    a.checked_sub(b).ok_or(err(Error::MathOverflow))
}
fn inc(d: &mut [u8], o: usize, v: u64) -> ProgramResult {
    let x = add(u64_at(d, o)?, v)?;
    put(d, o, &x.to_le_bytes())
}
fn open(cd: &mut [u8], up: bool) -> ProgramResult {
    let n = u32_at(cd, c::OPEN)?;
    let n = if up { n.checked_add(1).ok_or(err(Error::MathOverflow))? } else { n.saturating_sub(1) };
    put(cd, c::OPEN, &n.to_le_bytes())
}
/// a * b / BPS, b <= BPS
fn part(a: u64, b: u64) -> u64 {
    ((a as u128 * b as u128) / BPS as u128) as u64
}
fn writable(a: &AccountView) -> ProgramResult {
    need(a.is_writable(), Error::BadAccount)
}
fn signer(a: &AccountView) -> ProgramResult {
    if a.is_signer() {
        Ok(())
    } else {
        Err(ProgramError::MissingRequiredSignature)
    }
}
fn is(a: &AccountView, x: &Address) -> ProgramResult {
    need(a.address() == x, Error::BadAccount)
}
fn find(seeds: &[&[u8]], a: &AccountView) -> Result<u8, ProgramError> {
    let (address, bump) = Address::find_program_address(seeds, &ID);
    need(a.address() == &address, Error::BadAccount)?;
    Ok(bump)
}
fn check_pda(a: &AccountView, seeds: &[&[u8]]) -> ProgramResult {
    let address = Address::create_program_address(seeds, &ID).map_err(|_| err(Error::BadAccount))?;
    need(a.address() == &address, Error::BadAccount)
}
fn load<const N: usize>(a: &AccountView, disc: u8) -> Result<[u8; N], ProgramError> {
    if !a.owned_by(&ID) {
        return Err(ProgramError::IllegalOwner);
    }
    if a.data_len() != N {
        return Err(ProgramError::InvalidAccountData);
    }
    let d: [u8; N] = rd(&a.try_borrow()?, 0)?;
    need(d[0] == disc, Error::BadAccount)?;
    Ok(d)
}
fn config(a: &AccountView) -> Result<[u8; c::LEN], ProgramError> {
    let d = load(a, CONFIG)?;
    check_pda(a, &[b"config", &[d[c::BUMP]]])?;
    Ok(d)
}
fn coin(a: &AccountView) -> Result<[u8; k::LEN], ProgramError> {
    let d = load(a, COIN)?;
    check_pda(a, &[b"coin", &d[k::MINT..k::MINT + 32], &[d[k::BUMP]]])?;
    Ok(d)
}
fn admin_of(admin: &AccountView, cd: &[u8]) -> ProgramResult {
    signer(admin)?;
    need(admin.address() == &key(cd, c::ADMIN)?, Error::Unauthorized)
}
/// The coin's inbox: a data-less system account at ["inbox", mint].
fn inbox(a: &AccountView, kd: &[u8]) -> ProgramResult {
    need(a.owned_by(&SYSTEM) && a.data_len() == 0, Error::BadAccount)?;
    check_pda(a, &[b"inbox", &kd[k::MINT..k::MINT + 32], &[kd[k::IBUMP]]])
}
fn save(a: &mut AccountView, d: &[u8]) -> ProgramResult {
    writable(a)?;
    let mut data = a.try_borrow_mut()?;
    need(data.len() == d.len(), Error::BadAccount)?;
    data.copy_from_slice(d);
    Ok(())
}
fn live(cfg: &[u8]) -> ProgramResult {
    need(cfg[c::PAUSED] == 0, Error::Paused)
}
fn rent(len: usize) -> Result<u64, ProgramError> {
    Rent::get()?.try_minimum_balance(len)
}
/// What the inbox keeps between distributes: its own rent and the rent of its WSOL account (closed every time).
fn reserve() -> Result<u64, ProgramError> {
    add(rent(0)?, rent(TOKEN_LEN)?)
}
fn now() -> Result<i64, ProgramError> {
    Ok(Clock::get()?.unix_timestamp)
}
fn slot(i: usize) -> usize {
    k::SLOTS + i * s::LEN
}
fn init_account(payer: &AccountView, new: &AccountView, len: usize, seeds: &[Seed]) -> ProgramResult {
    signer(payer)?;
    writable(payer)?;
    writable(new)?;
    // Never overwrite records; a prefunded empty address is still usable (anti-griefing).
    if !new.owned_by(&SYSTEM) || new.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let shortfall = rent(len)?.saturating_sub(new.lamports());
    if shortfall > 0 {
        Transfer { from: payer, to: new, lamports: shortfall }.invoke()?;
    }
    let signers = [Signer::from(seeds)];
    Allocate { account: new, space: len as u64 }.invoke_signed(&signers)?;
    Assign { account: new, owner: &ID }.invoke_signed(&signers)
}
/// Move lamports out of an account this program owns.
fn pay(from: &mut AccountView, to: &mut AccountView, amount: u64) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    writable(from)?;
    writable(to)?;
    need(from.address() != to.address(), Error::BadAccount)?;
    let left = sub(from.lamports(), amount)?;
    let got = add(to.lamports(), amount)?;
    from.set_lamports(left);
    to.set_lamports(got);
    Ok(())
}
/// An address the runtime can never credit: executable, sysvar-owned, reserved id, or one this instruction cannot write.
fn uncreditable(a: &AccountView) -> bool {
    !a.is_writable() || a.executable() || a.owned_by(&SYSVAR) || RESERVED.contains(a.address())
}
/// Can `to` take `amount`? The runtime refuses a credit that leaves an account below the rent-exempt minimum for its size.
fn takes(to: &AccountView, amount: u64) -> Result<bool, ProgramError> {
    Ok(!uncreditable(to) && (amount == 0 || add(to.lamports(), amount)? >= rent(to.data_len())?))
}
/// A payee may not be zero, this program, the system program, the config, or the coin's own state.
fn payee_ok(x: &Address, cfg: &Address, coin_acc: &Address, inb: &Address) -> bool {
    *x != ZERO && x != &ID && *x != SYSTEM && x != cfg && x != coin_acc && x != inb
}
/// Dues of a slot unpaid for the period belong to the sink (`expire`).
fn overdue(cd: &[u8], kd: &[u8], o: usize, now: i64) -> Result<bool, ProgramError> {
    Ok(cd[c::SWEEP] == 1 && now.saturating_sub(i64_at(kd, o + s::SINCE)?) >= i64_at(cd, c::PERIOD)?)
}
/// What is left of today's cap on server-directed payouts (resets at 00:00 UTC).
fn cap_left(cd: &mut [u8], now: i64) -> Result<u64, ProgramError> {
    let day = now / DAY;
    if i64_at(cd, c::DAY)? != day {
        put(cd, c::DAY, &day.to_le_bytes())?;
        put(cd, c::USED, &0u64.to_le_bytes())?;
    }
    Ok(u64_at(cd, c::CAP)?.saturating_sub(u64_at(cd, c::USED)?))
}

/// One CPI. `acc` = (account, writable, signer); the inbox signs through `seeds`.
/// One body for every CPI (a generic per account count would copy it 7 times: rent is paid per byte).
fn call<const N: usize>(program: &Address, data: &[u8], acc: [(&AccountView, bool, bool); N], seeds: &[Seed]) -> ProgramResult {
    call_slice(program, data, &acc, seeds)
}
#[inline(never)]
fn call_slice(program: &Address, data: &[u8], acc: &[(&AccountView, bool, bool)], seeds: &[Seed]) -> ProgramResult {
    const MAX: usize = 18;
    need(!acc.is_empty() && acc.len() <= MAX, Error::BadAccount)?;
    let at = |i: usize| acc[i.min(acc.len() - 1)];
    let metas: [InstructionAccount; MAX] = core::array::from_fn(|i| InstructionAccount::new(at(i).0.address(), at(i).1, at(i).2));
    let views: [&AccountView; MAX] = core::array::from_fn(|i| at(i).0);
    let ix = InstructionView { program_id: program, data, accounts: &metas[..acc.len()] };
    invoke_signed_with_bounds::<MAX, _>(&ix, &views[..acc.len()], &[Signer::from(seeds)])
}
fn inbox_seeds<'a>(kd: &'a [u8], ibump: &'a [u8; 1]) -> [Seed<'a>; 3] {
    [Seed::from(b"inbox"), Seed::from(&kd[k::MINT..k::MINT + 32]), Seed::from(ibump)]
}

/// Bytes cursor over instruction args.
struct Cur<'a>(&'a [u8], usize);
impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProgramError> {
        let end = self.1.checked_add(n).ok_or(ProgramError::InvalidInstructionData)?;
        let v = self.0.get(self.1..end).ok_or(ProgramError::InvalidInstructionData)?;
        self.1 = end;
        Ok(v)
    }
    fn u8(&mut self) -> Result<u8, ProgramError> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64, ProgramError> {
        Ok(u64::from_le_bytes(rd(self.take(8)?, 0)?))
    }
    fn str(&mut self) -> Result<&'a [u8], ProgramError> {
        let n = self.u8()? as usize;
        self.take(n)
    }
}
/// Bytes writer for CPI data.
struct Buf<const N: usize>([u8; N], usize);
impl<const N: usize> Buf<N> {
    fn new(disc: [u8; 8]) -> Self {
        let mut b = Buf([0; N], 0);
        b.0[..8].copy_from_slice(&disc);
        b.1 = 8;
        b
    }
    fn raw(&mut self, v: &[u8]) -> ProgramResult {
        put(&mut self.0, self.1, v)?;
        self.1 += v.len();
        Ok(())
    }
    fn u64(&mut self, v: u64) -> ProgramResult {
        self.raw(&v.to_le_bytes())
    }
    fn str(&mut self, v: &[u8]) -> ProgramResult {
        self.raw(&(v.len() as u32).to_le_bytes())?;
        self.raw(v)
    }
    fn bytes(&self) -> &[u8] {
        &self.0[..self.1]
    }
}

pub fn process_instruction(pid: &Address, a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    need(pid == &ID, Error::Unauthorized)?;
    let (&tag, args) = d.split_first().ok_or(ProgramError::InvalidInstructionData)?;
    match tag {
        0 => init_config(a),
        1 => set_config(a, args),
        2 => set_coin_mint(a, args),
        3 => launch(a, args),
        4 => create(a, args),
        5 => distribute(a, args),
        6 => bind(a, args),
        7 => claim(a, args),
        8 => claim_platform(a),
        9 => expire(a, args),
        10 => retire(a, args),
        11 => refund_coin(a),
        12 => close_ticker(a),
        13 => close_acct(a),
        14 => close_config(a),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// Roles may not be this program, its config, the system program or zero; payout wallets must be creditable and not
/// our state; the CPMM config must be one. The admin signs to accept.
/// Wallets: buyback, team, holders, sink.
fn roles(admin: &AccountView, binder: &AccountView, wallets: [&AccountView; 4], cp: &AccountView, cfg: &Address) -> ProgramResult {
    signer(admin)?;
    need(*admin.address() != ZERO && admin.address() != &ID && admin.address() != cfg && *admin.address() != SYSTEM, Error::BadParams)?;
    need(*binder.address() != ZERO && binder.address() != &ID && binder.address() != cfg && *binder.address() != SYSTEM, Error::BadParams)?;
    for w in wallets {
        let x = w.address();
        need(*x != ZERO && x != &ID && x != cfg && *x != SYSTEM, Error::BadParams)?;
        need(!w.executable() && !w.owned_by(&SYSVAR) && !w.owned_by(&ID) && !RESERVED.contains(x), Error::BadParams)?;
    }
    need(cp.owned_by(&CPMM), Error::BadParams)
}
fn set_roles(cd: &mut [u8], admin: &AccountView, binder: &AccountView, w: [&AccountView; 4], cp: &AccountView) -> ProgramResult {
    put(cd, c::ADMIN, admin.address().as_ref())?;
    put(cd, c::BINDER, binder.address().as_ref())?;
    put(cd, c::BUYBACK, w[0].address().as_ref())?;
    put(cd, c::TEAM, w[1].address().as_ref())?;
    put(cd, c::HOLDERS, w[2].address().as_ref())?;
    put(cd, c::SINK, w[3].address().as_ref())?;
    put(cd, c::CP_CONFIG, cp.address().as_ref())
}

/// [authority(S,W), config(W), program, programdata, system, admin(S), binder, buyback, team, holders, cp_config].
/// Only the program's upgrade authority may create the config (no init front-running). Starts paused.
fn init_config(a: &mut [AccountView]) -> ProgramResult {
    let [authority, cfg, program, pd, sys, admin, binder, buyback, team, holders, cp, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    signer(authority)?;
    is(sys, &SYSTEM)?;
    need(program.address() == &ID && program.owned_by(&LOADER) && program.executable(), Error::Unauthorized)?;
    let pd_key = Address::find_program_address(&[ID.as_ref()], &LOADER).0;
    need(pd.address() == &pd_key && pd.owned_by(&LOADER), Error::Unauthorized)?;
    {
        let data = program.try_borrow()?;
        need(rd::<4>(&data, 0)? == 2u32.to_le_bytes() && key(&data, 4)? == pd_key, Error::Unauthorized)?;
        let data = pd.try_borrow()?;
        need(
            rd::<4>(&data, 0)? == 3u32.to_le_bytes() && byte(&data, 12)? == 1 && key(&data, 13)? == *authority.address(),
            Error::Unauthorized,
        )?;
    }
    let bump = [find(&[b"config"], cfg)?];
    roles(admin, binder, [buyback, team, holders, buyback], cp, cfg.address())?;
    init_account(authority, cfg, c::LEN, &[Seed::from(b"config"), Seed::from(&bump)])?;
    let mut cd = [0u8; c::LEN];
    cd[0] = CONFIG;
    cd[c::BUMP] = bump[0];
    set_roles(&mut cd, admin, binder, [buyback, team, holders, buyback], cp)?; // sink = buyback until set_config names one
    put(&mut cd, c::PLAT_BPS, &PLAT_BPS.to_le_bytes())?;
    put(&mut cd, c::BUYBACK_BPS, &BUYBACK_BPS.to_le_bytes())?;
    put(&mut cd, c::TEAM_BPS, &TEAM_BPS.to_le_bytes())?;
    cd[c::PAUSED] = 1; // opened by the admin when ready
    cd[c::SWEEP] = 1;
    cd[c::KIND_MAX] = KIND_MAX;
    put(&mut cd, c::CAP, &CAP.to_le_bytes())?;
    put(&mut cd, c::TIP, &TIP.to_le_bytes())?;
    put(&mut cd, c::PERIOD, &PERIOD.to_le_bytes())?;
    put(&mut cd, c::MIN_FEE, &MIN_FEE.to_le_bytes())?;
    put(&mut cd, c::MAX_FEE, &MAX_FEE.to_le_bytes())?;
    save(cfg, &cd)
}

/// [admin(S), config(W), new_admin(S) (may be the same key), binder, buyback, team, holders, cp_config, sink];
/// args: plat_bps u16 (<= 5000), buyback_bps u16, team_bps u16 (sum <= 10 000), paused u8, sweep u8, kind_max u8
/// (1..=8), cap u64, tip u64 (<= 50 000), period i64 (>= 60 s), min_fee u64, max_fee u64 (<= 5 %), lp_scale u64 (<= 1e6).
fn set_config(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [admin, cfg, new_admin, binder, buyback, team, holders, cp, sink, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    admin_of(admin, &cd)?;
    let mut r = Cur(d, 0);
    let plat_bps = u16::from_le_bytes(rd(r.take(2)?, 0)?);
    let buyback_bps = u16::from_le_bytes(rd(r.take(2)?, 0)?);
    let team_bps = u16::from_le_bytes(rd(r.take(2)?, 0)?);
    let (paused, sweep, kind_max) = (r.u8()?, r.u8()?, r.u8()?);
    let (cap, tip) = (r.u64()?, r.u64()?);
    let period = r.u64()? as i64;
    let (min_fee, max_fee, lp_scale) = (r.u64()?, r.u64()?, r.u64()?);
    need(
        plat_bps <= MAX_PLAT_BPS
            && buyback_bps as u64 + team_bps as u64 <= BPS
            && paused <= 1
            && sweep <= 1
            && (1..=MAX_KINDS).contains(&kind_max)
            && tip <= MAX_TIP
            && period >= MIN_PERIOD
            && min_fee <= max_fee
            && max_fee <= FEE_CAP
            && lp_scale <= LP_ALL,
        Error::BadParams,
    )?;
    let same = *new_admin.address() == key(&cd, c::ADMIN)?
        && *binder.address() == key(&cd, c::BINDER)?
        && *buyback.address() == key(&cd, c::BUYBACK)?
        && *team.address() == key(&cd, c::TEAM)?
        && *holders.address() == key(&cd, c::HOLDERS)?
        && *cp.address() == key(&cd, c::CP_CONFIG)?
        && *sink.address() == key(&cd, c::SINK)?;
    if same {
        // unchanged roles are not re-validated, so a pause (the first wind-down step) never stalls on them
        signer(new_admin)?;
    } else {
        roles(new_admin, binder, [buyback, team, holders, sink], cp, cfg.address())?;
    }
    set_roles(&mut cd, new_admin, binder, [buyback, team, holders, sink], cp)?;
    put(&mut cd, c::PLAT_BPS, &plat_bps.to_le_bytes())?;
    put(&mut cd, c::BUYBACK_BPS, &buyback_bps.to_le_bytes())?;
    put(&mut cd, c::TEAM_BPS, &team_bps.to_le_bytes())?;
    cd[c::PAUSED] = paused;
    cd[c::SWEEP] = sweep;
    cd[c::KIND_MAX] = kind_max;
    put(&mut cd, c::CAP, &cap.to_le_bytes())?;
    put(&mut cd, c::TIP, &tip.to_le_bytes())?;
    put(&mut cd, c::PERIOD, &period.to_le_bytes())?;
    put(&mut cd, c::MIN_FEE, &min_fee.to_le_bytes())?;
    put(&mut cd, c::MAX_FEE, &max_fee.to_le_bytes())?;
    put(&mut cd, c::LP_SCALE, &lp_scale.to_le_bytes())?;
    save(cfg, &cd)
}

/// [admin(S), config(W)]; args: mint. Once: the $PRISM address is set on coin day.
fn set_coin_mint(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [admin, cfg, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    admin_of(admin, &cd)?;
    need(key(&cd, c::COIN_MINT)? == ZERO, Error::BadParams)?;
    let mint = key(d, 0)?;
    need(mint != ZERO, Error::BadParams)?;
    put(&mut cd, c::COIN_MINT, mint.as_ref())?;
    save(cfg, &cd)
}

/// [author(S,W), mint(S), config(W), coin(W), inbox(W), ticker(W), system, launchlab, platform(W), cp_config, team].
/// args: fee u64 (1e-6, config bounds), n u8 (1..=5), n x (kind u8, key: 32 bytes for a wallet / u64 id for an account,
/// bps u16; sum 10 000), symbol (u8 length + 1..=10 bytes).
/// Records the recipients, locks the ticker for 60 min (the same symbol, ASCII case-folded, after each launch) and makes
/// the coin's LaunchLab platform: admin = fee wallet = the inbox, fee = `fee`, LP to the team by LP_SCALE, the rest
/// burned. `create` (+ the creator's first buy) follows in the same tx, or in the next one when the data does not fit.
/// The mint keypair signs: only the launcher can record a mint.
fn launch(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [author, mint, cfg, coin_acc, inb, ticker, sys, ll, platform, cp, team, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    live(&cd)?;
    signer(author)?;
    signer(mint)?;
    is(sys, &SYSTEM)?;
    is(ll, &LAUNCHLAB)?;
    is(cp, &key(&cd, c::CP_CONFIG)?)?;
    is(team, &key(&cd, c::TEAM)?)?;
    let mut r = Cur(d, 0);
    let fee = r.u64()?;
    need(fee >= u64_at(&cd, c::MIN_FEE)? && fee <= u64_at(&cd, c::MAX_FEE)?, Error::BadParams)?;
    let n = r.u8()? as usize;
    need((1..=MAX_SLOTS).contains(&n), Error::BadParams)?;

    let m = mint.address().as_ref();
    let bump = [find(&[b"coin", m], coin_acc)?];
    let ibump = [find(&[b"inbox", m], inb)?];
    let mut kd = [0u8; k::LEN];
    kd[0] = COIN;
    kd[k::BUMP] = bump[0];
    kd[k::IBUMP] = ibump[0];
    kd[k::N] = n as u8;
    put(&mut kd, k::PLAT_BPS, &rd::<2>(&cd, c::PLAT_BPS)?)?;
    put(&mut kd, k::MINT, m)?;
    put(&mut kd, k::AUTHOR, author.address().as_ref())?;
    put(&mut kd, k::PLATFORM, platform.address().as_ref())?;
    let now = now()?;
    let mut sum = 0u64;
    for i in 0..n {
        let o = slot(i);
        let kind = r.u8()?;
        need(kind <= cd[c::KIND_MAX], Error::BadParams)?;
        let mut id = [0u8; 32];
        let len = if kind == WALLET { 32 } else { 8 };
        id[..len].copy_from_slice(r.take(len)?);
        let bps = u16::from_le_bytes(rd(r.take(2)?, 0)?);
        need(bps > 0 && id != [0; 32], Error::BadParams)?;
        for j in 0..i {
            let p = slot(j);
            need(!(kd[p + s::KIND] == kind && kd[p + s::KEY..p + s::KEY + 32] == id), Error::BadParams)?;
        }
        sum += bps as u64;
        kd[o + s::KIND] = kind;
        put(&mut kd, o + s::KEY, &id)?;
        put(&mut kd, o + s::BPS, &bps.to_le_bytes())?;
        put(&mut kd, o + s::SINCE, &now.to_le_bytes())?;
        if kind == WALLET {
            let w = Address::new_from_array(id);
            need(payee_ok(&w, cfg.address(), coin_acc.address(), inb.address()), Error::BadParams)?;
            put(&mut kd, o + s::OWNER, &id)?;
        }
    }
    need(sum == BPS, Error::BadParams)?;
    let symbol = r.str()?;
    // A-Z a-z 0-9 only: look-alike letters, spaces or invisible characters would slip a twin past the lock
    need((1..=10).contains(&symbol.len()) && symbol.iter().all(|b| b.is_ascii_alphanumeric()), Error::BadParams)?;
    kd[k::SYM] = symbol.len() as u8;
    put(&mut kd, k::SYM + 1, symbol)?;

    let mut tk = [0u8; 10];
    for (x, y) in tk.iter_mut().zip(symbol) {
        *x = y.to_ascii_uppercase();
    }
    let tk = &tk[..symbol.len()];
    let tbump = [find(&[b"ticker", tk], ticker)?];
    // launch only reserves the ticker for WINDOW (the lock then looks expired); `create` starts the 60 min. A launch
    // that never creates holds it 2 min, and two pools of a ticker are always >= 60 min apart.
    let until = now.saturating_add(WINDOW).saturating_sub(LOCK);
    if ticker.owned_by(&ID) {
        let mut td: [u8; t::LEN] = load(ticker, TICKER)?;
        need(now.saturating_sub(i64_at(&td, t::TIME)?) >= LOCK, Error::TickerLocked)?;
        put(&mut td, t::TIME, &until.to_le_bytes())?;
        save(ticker, &td)?;
    } else {
        init_account(author, ticker, t::LEN, &[Seed::from(b"ticker"), Seed::from(tk), Seed::from(&tbump)])?;
        let mut td = [0u8; t::LEN];
        td[0] = TICKER;
        td[t::BUMP] = tbump[0];
        put(&mut td, t::TIME, &until.to_le_bytes())?;
        put(&mut td, t::PAYER, author.address().as_ref())?;
        save(ticker, &td)?;
        open(&mut cd, true)?;
    }
    put(&mut kd, k::TIME, &now.to_le_bytes())?;
    init_account(author, coin_acc, k::LEN, &[Seed::from(b"coin"), Seed::from(m), Seed::from(&bump)])?;
    save(coin_acc, &kd)?;
    open(&mut cd, true)?;
    let coins = u32_at(&cd, c::COINS)?.checked_add(1).ok_or(err(Error::MathOverflow))?;
    put(&mut cd, c::COINS, &coins.to_le_bytes())?;
    save(cfg, &cd)?;

    // the inbox pays the platform rent and keeps its reserve
    writable(inb)?;
    need(inb.owned_by(&SYSTEM) && inb.data_len() == 0, Error::BadAccount)?;
    let want = add(reserve()?, rent(PLATFORM_LEN)?)?.saturating_sub(inb.lamports());
    if want > 0 {
        Transfer { from: author, to: inb, lamports: want }.invoke()?;
    }
    let (inb, sys, platform, cp, team) = (&*inb, &*sys, &*platform, &*cp, &*team);
    let lp = u64_at(&cd, c::LP_SCALE)?;
    let mut b = Buf::<80>::new([176, 90, 196, 175, 253, 113, 220, 20]); // create_platform_config
    b.u64(lp)?; // platform_scale -> team NFT
    b.u64(0)?; // creator_scale
    b.u64(LP_ALL - lp)?; // burn_scale
    b.u64(fee)?;
    b.str(b"Prisma")?;
    b.str(b"")?;
    b.str(b"")?;
    b.u64(0)?; // creator_fee_rate: the whole fee is the platform's, i.e. the coin's
    b.u64(0)?; // platform_vesting_scale
    call(
        &LAUNCHLAB,
        b.bytes(),
        [(inb, true, true), (inb, false, false), (team, false, false), (platform, true, false), (cp, false, false), (sys, false, false), (inb, false, false), (inb, false, false)],
        &inbox_seeds(&kd, &ibump),
    )
}

/// [author(S,W), mint(S,W), config, coin, inbox, launchlab, platform, global_config, ll_authority, pool(W), wsol,
///  base_vault(W), quote_vault(W), metadata(W), token, metaplex, system, rent, event_authority, ticker(W)].
/// args: supply u64, sell u64, raise u64, name, symbol, uri (u8 length + bytes; the symbol as recorded by `launch`).
/// Within WINDOW of its `launch`: creates the LaunchLab pool of a recorded coin with the inbox as creator, on the coin's
/// own platform, migrating to CPMM, no vesting, the CPMM creator fee in SOL, and starts the ticker's 60 min lock. The
/// creator's first buy is a plain LaunchLab buy right after it.
fn create(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [author, mint, cfg, coin_acc, inb, ll, platform, global, ll_auth, pool, wsol, base_vault, quote_vault, meta, tok, metaplex, sys, rent_sv, event, ticker, ..] =
        a
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    live(&config(cfg)?)?;
    signer(author)?;
    signer(mint)?;
    let kd = coin(coin_acc)?;
    is(mint, &key(&kd, k::MINT)?)?;
    is(author, &key(&kd, k::AUTHOR)?)?;
    inbox(inb, &kd)?;
    is(ll, &LAUNCHLAB)?;
    is(platform, &key(&kd, k::PLATFORM)?)?;
    is(wsol, &WSOL)?;
    is(tok, &TOKEN)?;
    is(sys, &SYSTEM)?;
    let mut r = Cur(d, 0);
    let (supply, sell, raise) = (r.u64()?, r.u64()?, r.u64()?);
    let (name, symbol, uri) = (r.str()?, r.str()?, r.str()?);
    need(name.len() <= 32 && uri.len() <= 200 && symbol == &kd[k::SYM + 1..k::SYM + 1 + kd[k::SYM] as usize], Error::BadParams)?;
    let now = now()?;
    need(now.saturating_sub(i64_at(&kd, k::TIME)?) < WINDOW, Error::Expired)?; // else launch again
    let mut tk = [0u8; 10];
    for (x, y) in tk.iter_mut().zip(symbol) {
        *x = y.to_ascii_uppercase();
    }
    let mut td: [u8; t::LEN] = load(ticker, TICKER)?;
    check_pda(ticker, &[b"ticker", &tk[..symbol.len()], &[td[t::BUMP]]])?;
    put(&mut td, t::TIME, &now.to_le_bytes())?;
    save(ticker, &td)?;
    let mut b = Buf::<320>::new([67, 153, 175, 39, 218, 16, 38, 32]); // initialize_v2
    b.raw(&[6])?; // decimals
    b.str(name)?;
    b.str(symbol)?;
    b.str(uri)?;
    b.raw(&[0])?; // CurveParams::Constant
    b.u64(supply)?;
    b.u64(sell)?;
    b.u64(raise)?;
    b.raw(&[1])?; // migrate_type: CPMM (the creator fee after migration is ours to split)
    b.u64(0)?; // no vesting: it would lock tokens to the inbox
    b.u64(0)?;
    b.u64(0)?;
    b.raw(&[0])?; // AmmCreatorFeeOn::QuoteToken: the CPMM creator fee is paid in SOL
    let ib = [kd[k::IBUMP]];
    call(
        &LAUNCHLAB,
        b.bytes(),
        [
            (author, true, true),
            (inb, false, false),
            (global, false, false),
            (platform, false, false),
            (ll_auth, false, false),
            (pool, true, false),
            (mint, true, true),
            (wsol, false, false),
            (base_vault, true, false),
            (quote_vault, true, false),
            (meta, true, false),
            (tok, false, false),
            (tok, false, false),
            (metaplex, false, false),
            (sys, false, false),
            (rent_sv, false, false),
            (event, false, false),
            (ll, false, false),
        ],
        &inbox_seeds(&kd, &ib),
    )
}

/// Split `amount` (already in the coin account): the platform share is returned (the caller moves it to the config),
/// the rest goes to the slot dues by bps; the last slot takes the rounding dust. A slot owed nothing until now starts
/// its unpaid clock now (14 days unpaid counts from when money first waited, not from launch).
fn split(kd: &mut [u8], amount: u64, now: i64) -> Result<u64, ProgramError> {
    let plat = part(amount, u16_at(kd, k::PLAT_BPS)? as u64);
    let rest = sub(amount, plat)?;
    let n = kd[k::N] as usize;
    let mut given = 0u64;
    for i in 0..n {
        let o = slot(i);
        let share = if i + 1 == n { sub(rest, given)? } else { part(rest, u16_at(kd, o + s::BPS)? as u64) };
        given = add(given, share)?;
        if share > 0 && u64_at(kd, o + s::DUE)? == 0 {
            put(kd, o + s::SINCE, &now.to_le_bytes())?;
        }
        inc(kd, o + s::DUE, share)?;
    }
    inc(kd, k::TOTAL, amount)?;
    Ok(plat)
}
/// Move everything on the inbox above its reserve into the coin account.
fn pull(inb: &AccountView, coin_acc: &AccountView, kd: &[u8]) -> Result<u64, ProgramError> {
    let amount = inb.lamports().saturating_sub(reserve()?);
    if amount > 0 {
        let ib = [kd[k::IBUMP]];
        Transfer { from: inb, to: coin_acc, lamports: amount }.invoke_signed(&[Signer::from(&inbox_seeds(kd, &ib))])?;
    }
    Ok(amount)
}

/// Permissionless (our keeper, every 2 min when the coin's fee vaults pass its threshold). Works while paused.
/// [config(W), coin(W), inbox(W), wsol_account(W) (the inbox's WSOL ATA), wsol, system, token, ata_program, caller(S,W),
///  then one payee per slot (the wallet / the bound owner; anything for an unbound account),
///  then if flags & 1 (LaunchLab platform fee): launchlab, fee_vault_authority, platform, platform_fee_vault(W),
///  then if flags & 2 (CPMM creator fee, after migration): cpmm, cpmm_authority, pool(W), token_0_vault(W),
///  token_1_vault(W), vault_0_mint, vault_1_mint, creator_token_0(W), creator_token_1(W), token_0_program,
///  token_1_program, amm_config, creator_fee_share]; args: flags u8.
/// Each group's callee fails when it has nothing to pay: the keeper sets a flag only above its threshold. The inbox
/// signs both claims; the money lands in its WSOL account, which is closed into the inbox, then booked. The caller gets
/// min(tip, 1 %) and pays the creator's token accounts the CPMM creates on its first collect.
fn distribute(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [cfg, coin_acc, inb, wacc, wsol, sys, tok, atap, caller, rest @ ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    // the caller pays the CPMM's token accounts and gets the tip: never the inbox (its seeds would sign for it)
    signer(caller)?;
    need(caller.address() != inb.address() && caller.address() != coin_acc.address(), Error::BadAccount)?;
    let mut cd = config(cfg)?;
    let mut kd = coin(coin_acc)?;
    inbox(inb, &kd)?;
    is(wsol, &WSOL)?;
    is(sys, &SYSTEM)?;
    is(tok, &TOKEN)?;
    is(atap, &ATA)?;
    let flags = byte(d, 0)?;
    let n = kd[k::N] as usize;
    need(rest.len() >= n, Error::BadAccount)?;
    let (payees, mut extra) = rest.split_at_mut(n);
    let ib = [kd[k::IBUMP]];
    let seeds = inbox_seeds(&kd, &ib);
    // the WSOL account exists before any claim: the inbox pays its rent from the reserve, not the caller
    call(&ATA, &[1], [(inb, true, true), (wacc, true, false), (inb, false, false), (wsol, false, false), (sys, false, false), (tok, false, false)], &seeds)?;
    if flags & 1 != 0 {
        let [ll, fauth, platform, vault, more @ ..] = extra else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        is(ll, &LAUNCHLAB)?;
        is(platform, &key(&kd, k::PLATFORM)?)?;
        // LaunchLab fails on an empty vault: skip it (a wrong vault with money fails in LaunchLab's own checks)
        let full = vault.owned_by(&TOKEN) && vault.data_len() == TOKEN_LEN && u64_at(&vault.try_borrow()?, 64)? > 0;
        if full {
            call(
            &LAUNCHLAB,
            &[117, 241, 198, 168, 248, 218, 80, 29], // claim_platform_fee_from_vault
            [(inb, true, true), (fauth, false, false), (platform, false, false), (vault, true, false), (wacc, true, false), (wsol, false, false), (tok, false, false), (sys, false, false), (atap, false, false)],
            &seeds,
            )?;
        }
        extra = more;
    }
    if flags & 2 != 0 {
        let [cpmm, cauth, pool, v0, v1, m0, m1, t0, t1, p0, p1, amm, share, ..] = extra else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        is(cpmm, &CPMM)?;
        // CPMM fails with nothing to collect: skip it (creator_fees_token_0/1 of the pool state)
        let owed = pool.owned_by(&CPMM) && {
            let pd = pool.try_borrow()?;
            u64_at(&pd, 397)? | u64_at(&pd, 405)? > 0
        };
        if owed {
        call(
            &CPMM,
            &[202, 202, 34, 83, 226, 122, 145, 229], // collect_creator_fee_permissionless: pays the pool creator = the inbox
            [
                (caller, true, true),
                (inb, false, false),
                (cauth, false, false),
                (pool, true, false),
                (v0, true, false),
                (v1, true, false),
                (m0, false, false),
                (m1, false, false),
                (t0, true, false),
                (t1, true, false),
                (p0, false, false),
                (p1, false, false),
                (atap, false, false),
                (sys, false, false),
                (amm, false, false),
                (share, false, false),
            ],
            &seeds,
        )?;
        }
    }
    // unwrap: close the WSOL account into the inbox
    call(&TOKEN, &[9], [(wacc, true, false), (inb, true, false), (inb, false, true)], &seeds)?;
    let amount = pull(inb, coin_acc, &kd)?;
    let now = now()?;
    if amount > 0 {
        let tip = u64_at(&cd, c::TIP)?.min(amount / 100);
        let tip = if caller.is_signer() && caller.address() != coin_acc.address() && takes(caller, tip)? { tip } else { 0 };
        pay(coin_acc, caller, tip)?;
        let plat = split(&mut kd, sub(amount, tip)?, now)?;
        pay(coin_acc, cfg, plat)?;
        inc(&mut cd, c::PLAT_DUE, plat)?;
    }
    // pay whoever can take it now; the rest waits as the slot's due
    for (i, payee) in payees.iter_mut().enumerate() {
        let o = slot(i);
        let owner = key(&kd, o + s::OWNER)?;
        let due = u64_at(&kd, o + s::DUE)?;
        // a binding naming our own state (server mistake) is never paid: it would stall every distribute
        let ours = !payee_ok(&owner, cfg.address(), coin_acc.address(), inb.address());
        // an owned slot must be passed its owner: a caller cannot hold someone's payout back by naming another account
        need(owner == ZERO || ours || payee.address() == &owner, Error::BadAccount)?;
        if due == 0 || owner == ZERO || ours || overdue(&cd, &kd, o, now)? {
            continue;
        }
        let bound = kd[o + s::FLAGS] & BOUND != 0;
        let amt = if bound { due.min(cap_left(&mut cd, now)?) } else { due };
        if amt == 0 || !takes(payee, amt)? {
            continue;
        }
        pay(coin_acc, payee, amt)?;
        if bound {
            inc(&mut cd, c::USED, amt)?;
        }
        put(&mut kd, o + s::DUE, &(due - amt).to_le_bytes())?;
        put(&mut kd, o + s::SINCE, &now.to_le_bytes())?;
    }
    save(coin_acc, &kd)?;
    save(cfg, &cd)
}

/// [binder(S,W), config(W), acct(W), system]; args: kind u8 (1..=kind_max), id u64, owner [32]. Our server, after the
/// person signed in with that account (the numeric id comes from the provider), binds it to their wallet. Once.
fn bind(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [binder, cfg, acct, sys, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    live(&cd)?;
    signer(binder)?;
    need(binder.address() == &key(&cd, c::BINDER)?, Error::Unauthorized)?;
    is(sys, &SYSTEM)?;
    let kind = byte(d, 0)?;
    let id: [u8; 8] = rd(d, 1)?;
    let owner = key(d, 9)?;
    need(kind != WALLET && kind <= cd[c::KIND_MAX] && id != [0; 8], Error::BadParams)?;
    need(owner != ZERO && owner != ID && owner != SYSTEM && owner != *cfg.address() && owner != *acct.address(), Error::BadParams)?;
    let bump = [find(&[b"acct", &[kind], &id], acct)?];
    if acct.owned_by(&ID) {
        return Err(err(Error::AlreadyBound));
    }
    init_account(binder, acct, ac::LEN, &[Seed::from(b"acct"), Seed::from(&[kind]), Seed::from(&id), Seed::from(&bump)])?;
    let mut ad = [0u8; ac::LEN];
    ad[0] = ACCT;
    ad[ac::BUMP] = bump[0];
    ad[ac::KIND] = kind;
    put(&mut ad, ac::ID, &id)?;
    put(&mut ad, ac::OWNER, owner.as_ref())?;
    put(&mut ad, ac::PAYER, binder.address().as_ref())?;
    save(acct, &ad)?;
    open(&mut cd, true)?;
    save(cfg, &cd)
}

/// Permissionless. [config(W), coin(W), acct (any account for a wallet slot), owner(W), caller(W)]; args: slot u8.
/// Pays the slot's due to its owner: a wallet slot's wallet, or the wallet its account is bound to (the first claim
/// copies the binding into the slot, later distributes pay it directly). Server-bound payouts stay within today's cap.
/// A caller other than the owner that signs (our server paying the fee) gets min(tip, 1 % of the payout).
fn claim(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [cfg, coin_acc, acct, owner, caller, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    let mut kd = coin(coin_acc)?;
    let i = byte(d, 0)? as usize;
    need(i < kd[k::N] as usize, Error::BadParams)?;
    let o = slot(i);
    if kd[o + s::KIND] != WALLET && key(&kd, o + s::OWNER)? == ZERO {
        let ad: [u8; ac::LEN] = load(acct, ACCT)?;
        check_pda(acct, &[b"acct", &[kd[o + s::KIND]], &kd[o + s::KEY..o + s::KEY + 8], &[ad[ac::BUMP]]])?;
        put(&mut kd, o + s::OWNER, &ad[ac::OWNER..ac::OWNER + 32])?;
        kd[o + s::FLAGS] |= BOUND;
    }
    need(key(&kd, o + s::OWNER)? == *owner.address(), Error::Unauthorized)?;
    let now = now()?;
    need(!overdue(&cd, &kd, o, now)?, Error::Expired)?; // it is the sink.s: `expire`
    let due = u64_at(&kd, o + s::DUE)?;
    let mut amount = due;
    if kd[o + s::FLAGS] & BOUND != 0 {
        amount = amount.min(cap_left(&mut cd, now)?);
        need(amount > 0 || due == 0, Error::CapReached)?;
        inc(&mut cd, c::USED, amount)?;
    }
    need(amount > 0, Error::NothingToClaim)?;
    // 1 % at most: a stranger calling claim cannot turn small dues into its own tips
    let tip = if caller.address() != owner.address() && caller.is_signer() { u64_at(&cd, c::TIP)?.min(amount / 100) } else { 0 };
    let tip = if tip > 0 && takes(caller, tip)? { tip } else { 0 };
    let net = sub(amount, tip)?;
    need(takes(owner, net)?, Error::PayoutBelowRent)?;
    pay(coin_acc, caller, tip)?;
    pay(coin_acc, owner, net)?;
    put(&mut kd, o + s::DUE, &(due - amount).to_le_bytes())?;
    put(&mut kd, o + s::SINCE, &now.to_le_bytes())?;
    save(coin_acc, &kd)?;
    save(cfg, &cd)
}

/// Permissionless. [config(W), buyback(W), team(W), holders(W)]: pays the platform money by the config's shares; a
/// wallet that cannot take its part now leaves it for later.
fn claim_platform(a: &mut [AccountView]) -> ProgramResult {
    let [cfg, buyback, team, holders, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    is(buyback, &key(&cd, c::BUYBACK)?)?;
    is(team, &key(&cd, c::TEAM)?)?;
    is(holders, &key(&cd, c::HOLDERS)?)?;
    let due = u64_at(&cd, c::PLAT_DUE)?;
    need(due > 0, Error::NothingToClaim)?;
    let b = part(due, u16_at(&cd, c::BUYBACK_BPS)? as u64);
    let t = part(due, u16_at(&cd, c::TEAM_BPS)? as u64);
    // all three or none: a wallet that cannot take its part must not shift it to the others
    for (to, amt) in [(&*buyback, b), (&*team, t), (&*holders, due - b - t)] {
        need(takes(to, amt)?, Error::PayoutBelowRent)?;
    }
    for (to, amt) in [(buyback, b), (team, t), (holders, due - b - t)] {
        pay(cfg, to, amt)?;
    }
    put(&mut cd, c::PLAT_DUE, &0u64.to_le_bytes())?;
    save(cfg, &cd)
}

/// Permissionless (our keeper). [config, coin(W), sink(W)]; args: slot u8. A slot whose due went unpaid for the period
/// pays the whole due to the config's sink wallet (the team buys back and burns with it; no tip: all of it is burned).
/// The unpaid clock restarts.
fn expire(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [cfg, coin_acc, sink, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let cd = config(cfg)?;
    is(sink, &key(&cd, c::SINK)?)?;
    let mut kd = coin(coin_acc)?;
    let i = byte(d, 0)? as usize;
    need(i < kd[k::N] as usize, Error::BadParams)?;
    let o = slot(i);
    let now = now()?;
    need(overdue(&cd, &kd, o, now)?, Error::NotExpired)?;
    let due = u64_at(&kd, o + s::DUE)?;
    need(due > 0, Error::NothingToClaim)?;
    need(takes(sink, due)?, Error::PayoutBelowRent)?;
    pay(coin_acc, sink, due)?;
    put(&mut kd, o + s::DUE, &0u64.to_le_bytes())?;
    put(&mut kd, o + s::SINCE, &now.to_le_bytes())?;
    save(coin_acc, &kd)
}

/// Wind-down, while paused. [admin(S), config, coin, inbox, launchlab, platform(W)]; args: what u8. The inbox, the
/// coin's platform admin, updates it: 0 = fee rate 0 (no more fees into a vault nobody will claim), 1 = the CPMM pool
/// creator after a future migration = the coin's author.
fn retire(a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [admin, cfg, coin_acc, inb, ll, platform, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let cd = config(cfg)?;
    admin_of(admin, &cd)?;
    need(cd[c::PAUSED] == 1, Error::NotPaused)?;
    let kd = coin(coin_acc)?;
    inbox(inb, &kd)?;
    is(ll, &LAUNCHLAB)?;
    is(platform, &key(&kd, k::PLATFORM)?)?;
    let mut b = Buf::<48>::new([195, 60, 76, 129, 146, 45, 67, 143]); // update_platform_config
    match byte(d, 0)? {
        0 => {
            b.raw(&[3])?; // FeeRate
            b.u64(0)?;
        }
        1 => {
            b.raw(&[11])?; // PlatformCPCreator
            b.raw(&kd[k::AUTHOR..k::AUTHOR + 32])?;
        }
        _ => return Err(err(Error::BadParams)),
    }
    let ib = [kd[k::IBUMP]];
    call(&LAUNCHLAB, b.bytes(), [(inb, false, true), (platform, true, false)], &inbox_seeds(&kd, &ib))
}

/// Wind-down, while paused, right after a `distribute` in the same tx. [admin(S), config(W), coin(W), inbox(W),
/// author(W), system, sink(W), then two accounts per slot in slot order: (payee, binding)]. The payee is the slot's
/// owner; for an account slot no claim has copied a binding into yet, the wallet its ["acct", kind, id] binding names
/// when that binding (passed next) exists, else the author. Books what is left on the inbox, pays every due (overdue ->
/// sink; one that cannot take it -> author, then the platform money); the inbox reserve and the coin rent go back to the
/// author; the coin closes.
fn refund_coin(a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, coin_acc, inb, author, sys, sink, owners @ ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    admin_of(admin, &cd)?;
    need(cd[c::PAUSED] == 1, Error::NotPaused)?;
    is(sys, &SYSTEM)?;
    is(sink, &key(&cd, c::SINK)?)?;
    writable(sink)?; // a read-only sink would let overdue dues fall through to the author
    let now = now()?;
    let mut kd = coin(coin_acc)?;
    inbox(inb, &kd)?;
    is(author, &key(&kd, k::AUTHOR)?)?;
    writable(author)?;
    let n = kd[k::N] as usize;
    need(owners.len() >= 2 * n, Error::BadAccount)?;
    let amount = pull(inb, coin_acc, &kd)?;
    let plat = split(&mut kd, amount, now)?;
    pay(coin_acc, cfg, plat)?;
    inc(&mut cd, c::PLAT_DUE, plat)?;
    let left = inb.lamports();
    if left > 0 {
        let ib = [kd[k::IBUMP]];
        Transfer { from: inb, to: author, lamports: left }.invoke_signed(&[Signer::from(&inbox_seeds(&kd, &ib))])?;
    }
    for (i, pair) in owners.chunks_mut(2).take(n).enumerate() {
        let [dest, binding] = pair else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        let o = slot(i);
        let due = u64_at(&kd, o + s::DUE)?;
        let mut owner = key(&kd, o + s::OWNER)?;
        if owner == ZERO && kd[o + s::KIND] != WALLET {
            // the canonical binding address, existing or not: the admin cannot skip a binding to pay the author
            find(&[b"acct", &[kd[o + s::KIND]], &kd[o + s::KEY..o + s::KEY + 8]], binding)?;
            if binding.owned_by(&ID) {
                owner = key(&load::<{ ac::LEN }>(binding, ACCT)?, ac::OWNER)?;
            }
        }
        is(dest, if owner == ZERO { author.address() } else { &owner })?;
        // a plain wallet must come writable: only addresses the runtime never lets write may be read-only
        need(dest.is_writable() || dest.executable() || dest.owned_by(&SYSVAR) || RESERVED.contains(dest.address()), Error::BadAccount)?;
        if overdue(&cd, &kd, o, now)? && takes(sink, due)? {
            pay(coin_acc, sink, due)?; // unpaid past the period: the sink's, as in `expire`
        } else if dest.address() != author.address() && payee_ok(dest.address(), cfg.address(), coin_acc.address(), inb.address()) && takes(dest, due)? {
            pay(coin_acc, dest, due)?;
        } else if takes(author, due)? {
            pay(coin_acc, author, due)?;
        } else {
            pay(coin_acc, cfg, due)?;
            inc(&mut cd, c::PLAT_DUE, due)?;
        }
        put(&mut kd, o + s::DUE, &0u64.to_le_bytes())?;
    }
    let rest = coin_acc.lamports();
    pay(coin_acc, author, rest)?;
    coin_acc.try_borrow_mut()?.fill(0);
    coin_acc.close()?;
    open(&mut cd, false)?;
    let coins = u32_at(&cd, c::COINS)?.saturating_sub(1);
    put(&mut cd, c::COINS, &coins.to_le_bytes())?;
    save(cfg, &cd)
}

/// Permissionless. [config(W), ticker(W), payer(W)]: a ticker lock past its 60 min (also while paused) closes; the
/// rent goes back to whoever paid it.
fn close_ticker(a: &mut [AccountView]) -> ProgramResult {
    let [cfg, ticker, payer, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    let td: [u8; t::LEN] = load(ticker, TICKER)?;
    is(payer, &key(&td, t::PAYER)?)?;
    need(now()?.saturating_sub(i64_at(&td, t::TIME)?) >= LOCK, Error::TickerLocked)?; // a pause is reversible: no early close
    close(ticker, payer)?;
    open(&mut cd, false)?;
    save(cfg, &cd)
}

/// Wind-down, while paused, after every coin is refunded (refunds read the bindings). [admin(S), config(W), acct(W),
/// payer(W)]: closes an account binding, rent to its payer.
fn close_acct(a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, acct, payer, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg)?;
    admin_of(admin, &cd)?;
    need(cd[c::PAUSED] == 1, Error::NotPaused)?;
    need(u32_at(&cd, c::COINS)? == 0, Error::AccountsOpen)?;
    let ad: [u8; ac::LEN] = load(acct, ACCT)?;
    is(payer, &key(&ad, ac::PAYER)?)?;
    close(acct, payer)?;
    open(&mut cd, false)?;
    save(cfg, &cd)
}
fn close(acc: &mut AccountView, to: &mut AccountView) -> ProgramResult {
    let all = acc.lamports();
    pay(acc, to, all)?;
    acc.try_borrow_mut()?.fill(0);
    acc.close()
}

/// [admin(S,W), config(W)]: paused, every coin / ticker / account closed and the platform money paid (`claim_platform`);
/// the rent back to the admin.
fn close_config(a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let cd = config(cfg)?;
    admin_of(admin, &cd)?;
    need(cd[c::PAUSED] == 1, Error::NotPaused)?;
    need(u32_at(&cd, c::OPEN)? == 0, Error::AccountsOpen)?;
    need(u64_at(&cd, c::PLAT_DUE)? == 0, Error::AccountsOpen)?;
    close(cfg, admin)
}
