//! Who is calling: local users by their peer credentials, other devices by token (plan 6.6), the
//! pairing that hands out tokens, and the one-time links that log browsers in.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use jiff::Timestamp;
use psst_proto::{DeviceId, api::Pairing};
use rand::RngExt;
use sha2::{Digest, Sha256};

/// A pairing code is valid this long.
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// Wrong redemptions after which every pending code is void.
pub const MAX_WRONG_ATTEMPTS: u32 = 5;
/// Characters of a pairing code.
pub const CODE_LEN: usize = 8;
/// Pairing code alphabet: no `0`/`O`, `1`/`I`/`L` or `U`, so codes read back unambiguously.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
/// Prefix of every device token, so a leaked one is recognisable.
pub const TOKEN_PREFIX: &str = "psst_";
/// A login link is valid this long, for one login.
pub const LINK_TTL: Duration = Duration::from_secs(60);
/// A browser session lasts this long from its login.
pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// The cookie that holds a browser's session.
pub const SESSION_COOKIE: &str = "psst_session";
/// Redemptions (of pairing codes, and of login links) allowed per source address, and from
/// everyone together, per window.
const REDEEM_PER_ADDR: u32 = 10;
const REDEEM_TOTAL: u32 = 30;
const REDEEM_WINDOW: Duration = Duration::from_secs(60);

/// An authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// A user on this machine, through the Unix socket. Always an admin.
    Local { user: String },
    /// A paired device, through TCP.
    Token { device_id: String },
}

impl Identity {
    pub fn device(&self) -> DeviceId {
        match self {
            Self::Local { user } => format!("local:{user}"),
            Self::Token { device_id } => device_id.clone(),
        }
    }
}

/// Why a caller isn't let in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No credentials, or unknown, revoked or expired ones.
    Unauthorized,
    /// A local user who isn't in `local_users`.
    Forbidden,
    /// A browser's state-changing request that didn't come from the web UI's own origin.
    CrossSite,
}

fn random_string(alphabet: &[u8], len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| alphabet[rng.random_range(0..alphabet.len())] as char).collect()
}

/// A new device token: `psst_` and 40 random alphanumerics (about 238 bits).
pub fn new_token() -> String {
    format!("{TOKEN_PREFIX}{}", random_string(TOKEN_ALPHABET, 40))
}

/// A secret for a login link or a browser session: 40 random alphanumerics (about 238 bits).
pub fn new_secret() -> String {
    random_string(TOKEN_ALPHABET, 40)
}

/// What the database keeps instead of the token: its SHA-256, in hex. Tokens are random, so a
/// fast hash is enough.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Pending pairing codes and the rate limit of their redemption.
#[derive(Debug, Default)]
pub struct Pairings {
    inner: Mutex<PairingState>,
}

#[derive(Debug, Default)]
struct PairingState {
    codes: Vec<PendingCode>,
    limit: RedeemLimit,
}

#[derive(Debug)]
struct PendingCode {
    code: String,
    name: String,
    expires: Instant,
    wrong: u32,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    count: u32,
}

impl Window {
    /// Counts one request; false if the window is already full.
    fn admit(slot: &mut Option<Window>, now: Instant, limit: u32) -> bool {
        let window = match slot {
            Some(window) if now.duration_since(window.start) < REDEEM_WINDOW => window,
            _ => slot.insert(Window { start: now, count: 0 }),
        };
        window.count += 1;
        window.count <= limit
    }
}

/// The rate limit of unauthenticated redemptions: [`REDEEM_PER_ADDR`] per source address and
/// [`REDEEM_TOTAL`] from everyone, per [`REDEEM_WINDOW`].
#[derive(Debug, Default)]
pub struct RedeemLimit {
    per_addr: HashMap<IpAddr, Window>,
    total: Option<Window>,
}

impl RedeemLimit {
    /// Counts one attempt from `addr`; false if it's over either limit.
    pub fn admit(&mut self, addr: IpAddr, now: Instant) -> bool {
        self.per_addr.retain(|_, window| now.duration_since(window.start) < REDEEM_WINDOW);
        let mut slot = self.per_addr.get(&addr).copied();
        let addr_ok = Window::admit(&mut slot, now, REDEEM_PER_ADDR);
        self.per_addr.insert(addr, slot.expect("admit fills the slot"));
        let total_ok = Window::admit(&mut self.total, now, REDEEM_TOTAL);
        addr_ok && total_ok
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemError {
    RateLimited,
    /// Unknown, expired or void.
    Invalid,
}

impl Pairings {
    /// A new one-time code for a device to be called `name`.
    pub fn create(&self, name: String) -> Pairing {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        state.codes.retain(|code| code.expires > now);
        let code = loop {
            let code = random_string(CODE_ALPHABET, CODE_LEN);
            if !state.codes.iter().any(|pending| pending.code == code) {
                break code;
            }
        };
        state.codes.push(PendingCode { code: code.clone(), name, expires: now + CODE_TTL, wrong: 0 });
        let expires_at = Timestamp::now() + CODE_TTL;
        let expires_at = expires_at.round(jiff::Unit::Second).unwrap_or(expires_at);
        Pairing { code, expires_at }
    }

    /// Redeems a code and returns the device name it was created for.
    ///
    /// A wrong code counts as a wrong attempt against every pending code, since nobody can tell
    /// which one was being guessed; a code with [`MAX_WRONG_ATTEMPTS`] is void.
    pub fn redeem(&self, addr: IpAddr, code: &str) -> Result<String, RedeemError> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if !state.limit.admit(addr, now) {
            return Err(RedeemError::RateLimited);
        }

        state.codes.retain(|code| code.expires > now);
        let wanted: String =
            code.chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| c.to_ascii_uppercase()).collect();
        if let Some(pos) = state.codes.iter().position(|pending| pending.code == wanted) {
            return Ok(state.codes.remove(pos).name);
        }
        for pending in &mut state.codes {
            pending.wrong += 1;
        }
        state.codes.retain(|pending| pending.wrong < MAX_WRONG_ATTEMPTS);
        Err(RedeemError::Invalid)
    }
}

/// Pending login links (plan 6.6), by the SHA-256 of their token, and the rate limit of their
/// redemption. They live only in memory: a link outlives neither its minute nor a restart.
#[derive(Debug, Default)]
pub struct LoginLinks {
    inner: Mutex<LinkState>,
}

#[derive(Debug, Default)]
struct LinkState {
    links: HashMap<String, PendingLink>,
    limit: RedeemLimit,
}

#[derive(Debug)]
struct PendingLink {
    /// Who asked for it: the session it gives belongs to this device.
    device: DeviceId,
    expires: Instant,
}

impl LoginLinks {
    /// A new link's token, valid for [`LINK_TTL`] from `now`, logging in for `device`.
    pub fn create(&self, device: DeviceId, now: Instant) -> String {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.links.retain(|_, link| link.expires > now);
        let token = new_secret();
        state.links.insert(hash_token(&token), PendingLink { device, expires: now + LINK_TTL });
        token
    }

    /// Uses up a link's token and returns the device it logs in for.
    pub fn redeem(&self, addr: IpAddr, token: &str, now: Instant) -> Result<DeviceId, RedeemError> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !state.limit.admit(addr, now) {
            return Err(RedeemError::RateLimited);
        }
        state.links.retain(|_, link| link.expires > now);
        state.links.remove(&hash_token(token.trim())).map(|link| link.device).ok_or(RedeemError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    const LAPTOP: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2));

    #[test]
    fn a_login_link_logs_in_once_within_its_minute() {
        let links = LoginLinks::default();
        let now = Instant::now();
        let token = links.create("d81f".into(), now);
        assert_eq!(links.redeem(LAPTOP, "not-the-token", now), Err(RedeemError::Invalid));
        assert_eq!(links.redeem(LAPTOP, &token, now + LINK_TTL - Duration::from_millis(1)), Ok("d81f".into()));
        assert_eq!(links.redeem(LAPTOP, &token, now + Duration::from_secs(2)), Err(RedeemError::Invalid));
    }

    #[test]
    fn a_login_link_expires_after_a_minute() {
        let links = LoginLinks::default();
        let now = Instant::now();
        let token = links.create("local:alice".into(), now);
        assert_eq!(links.redeem(LAPTOP, &token, now + LINK_TTL), Err(RedeemError::Invalid));
    }
}
