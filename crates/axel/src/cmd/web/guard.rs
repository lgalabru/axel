//! Who may talk to a web pane's server: only the page its own Ladybird
//! opened.
//!
//! - **The address carries a token, good once.**  Ladybird is started with
//!   `http://127.0.0.1:PORT/?k=TOKEN`, and on macOS any local process can
//!   read every process's arguments (`ps`).  So the token opens one page
//!   load within [`TOKEN_SECS`] and is gone: read from `ps` afterwards, it
//!   opens nothing.  Each Ladybird start gets a token of its own.
//! - **The page's proof is a cookie no command line carries.**  Redeeming
//!   the token sets [`COOKIE`], a second secret made when the server starts,
//!   `HttpOnly` (the page's script can't read it) and `SameSite=Strict`
//!   (no other site's page sends it).
//! - **Every request names this server** (`Host`), so a page elsewhere can't
//!   reach it through DNS rebinding.
//! - **Every write comes from this page**: a JSON body with this server's
//!   `Origin`, which a form or an image elsewhere can't send.
//!
//! Secrets are 32 bytes from the OS's random source, compared in constant
//! time.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// The cookie that proves a request comes from the pane's page.
pub const COOKIE: &str = "axel_web";
/// How long an address's token stays good: Ladybird asks for the page well
/// under a second after it starts.
pub const TOKEN_SECS: u64 = 30;
/// The most tokens waiting at once: one per Ladybird start, which come a
/// few at most between two loads.  Past it the oldest is dropped.
const TOKENS_MAX: usize = 8;
/// Bytes of randomness in each secret: 256 bits, past guessing.
const SECRET_BYTES: usize = 32;

/// A pane server's secrets.
#[derive(Debug)]
pub struct Guard {
    /// The page's cookie, for every request after the first.
    cookie: String,
    /// Tokens not redeemed yet, and when each was made.
    tokens: Mutex<Vec<(String, Instant)>>,
}

impl Guard {
    pub fn new() -> Result<Guard> {
        Ok(Guard { cookie: random_secret()?, tokens: Mutex::new(Vec::with_capacity(TOKENS_MAX)) })
    }

    /// A token for one address, good once within [`TOKEN_SECS`].
    pub fn mint(&self) -> Result<String> {
        let token = random_secret()?;
        let mut tokens = self.tokens.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        tokens.retain(|(_, made)| made.elapsed() < Duration::from_secs(TOKEN_SECS));
        if tokens.len() >= TOKENS_MAX {
            tokens.remove(0);
        }
        tokens.push((token.clone(), Instant::now()));
        assert!(tokens.len() <= TOKENS_MAX, "the tokens waiting are bounded");
        Ok(token)
    }

    /// Spend `token`: whether it was one of ours, fresh, and not spent.
    pub fn redeem(&self, token: &str) -> bool {
        let mut tokens = self.tokens.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = |made: &Instant| made.elapsed() < Duration::from_secs(TOKEN_SECS);
        let Some(at) = tokens.iter().position(|(t, made)| same(t, token) && fresh(made)) else { return false };
        tokens.remove(at);
        debug_assert!(!tokens.iter().any(|(t, _)| same(t, token)), "a token is spent once");
        true
    }

    /// The `Set-Cookie` value a redeemed token earns.
    pub fn set_cookie(&self) -> String {
        format!("{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/", self.cookie)
    }

    /// Whether a `Cookie` header carries the page's cookie.
    pub fn admits(&self, cookie_header: Option<&str>) -> bool {
        let Some(header) = cookie_header else { return false };
        header.split(';').filter_map(|pair| pair.trim().split_once('=')).any(|(name, value)| name == COOKIE && same(value, &self.cookie))
    }
}

/// Whether `host` (a `Host` header) names this server.
pub fn own_host(port: u16, host: Option<&str>) -> bool {
    host.is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"))
}

/// Whether `origin` (an `Origin` header) is this server's.  A write without
/// one is refused: every browser sends it with a `fetch` POST.
pub fn own_origin(port: u16, origin: Option<&str>) -> bool {
    origin.is_some_and(|o| o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}"))
}

/// Compares in time independent of where the first difference is.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// [`SECRET_BYTES`] from the OS's random source, as hex.
fn random_secret() -> Result<String> {
    let mut bytes = [0_u8; SECRET_BYTES];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).context("reading /dev/urandom")?;
    assert!(bytes.iter().any(|b| *b != 0), "32 random bytes are never all zero");
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_opens_one_page_load_and_never_the_cookie() {
        let guard = Guard::new().unwrap();
        let token = guard.mint().unwrap();
        assert_eq!(token.len(), SECRET_BYTES * 2);
        assert!(!guard.set_cookie().contains(&token), "the address's token is not the page's proof");
        assert!(guard.redeem(&token));
        assert!(!guard.redeem(&token), "read from ps after the load, it opens nothing");
        assert!(!guard.redeem("not-a-token"));
        assert!(!guard.admits(Some(&format!("{COOKIE}={token}"))), "a token is no cookie");
    }

    #[test]
    fn the_cookie_is_checked_by_name_and_value() {
        let guard = Guard::new().unwrap();
        let cookie = guard.set_cookie();
        let value = cookie.split(';').next().unwrap();
        assert!(guard.admits(Some(&format!("other=1; {value}"))));
        assert!(!guard.admits(Some(&value.replace(COOKIE, "nope"))));
        assert!(!guard.admits(None));
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
    }

    #[test]
    fn waiting_tokens_are_bounded_and_the_oldest_goes_first() {
        let guard = Guard::new().unwrap();
        let first = guard.mint().unwrap();
        for _ in 0..TOKENS_MAX {
            guard.mint().unwrap();
        }
        assert!(!guard.redeem(&first), "dropped to keep the waiting tokens bounded");
    }

    #[test]
    fn only_this_servers_host_and_origin_pass() {
        assert!(own_host(4000, Some("127.0.0.1:4000")) && own_host(4000, Some("localhost:4000")));
        assert!(!own_host(4000, Some("evil.example:4000")) && !own_host(4000, None));
        assert!(own_origin(4000, Some("http://127.0.0.1:4000")));
        assert!(!own_origin(4000, None), "a write must say where it comes from");
        assert!(!own_origin(4000, Some("http://127.0.0.1:4001")));
    }
}
