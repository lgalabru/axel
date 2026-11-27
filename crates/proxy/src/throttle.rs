//! A bad network, replayed on purpose: bandwidth, latency and loss applied
//! to every request the proxy forwards, so an airport or a train can be
//! tested at a desk without sudo or system tools (`AXEL_THROTTLE`).
//!
//! A profile is a preset name (`airport`), a spec
//! (`up=1mbit,down=5mbit,rtt=300ms,loss=2`), or a preset with overrides
//! (`airport,loss=0`).

use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::time::Sleep;

/// How a session's network is slowed down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Throttle {
    /// How fast request bodies go upstream; `None`: as fast as they can.
    pub up_bits_per_sec: Option<u64>,
    /// How fast responses stream back; `None`: as fast as they arrive.
    pub down_bits_per_sec: Option<u64>,
    /// Added each way: before a request goes upstream, and again before its
    /// response starts.  Half the round trip.
    pub latency_ms: u32,
    /// The share of requests lost, 0 to 100: the connection breaks before
    /// any response byte, as a real network would.
    pub loss_pct: u8,
}

/// The named profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThrottlePreset {
    /// A congested shared Wi-Fi at a gate, behind a captive portal: 1 Mbit/s
    /// up, 3 down, 400 ms round trip, 3% loss.  The owner's own profile of a
    /// bad airport connection, not a measurement; it sits between Firefox's
    /// "Good 3G" and Apple's Network Link Conditioner "Very Bad Network".
    Airport,
    /// On-board Wi-Fi shared by a carriage over a cellular uplink, with
    /// handovers between masts and tunnels: 512 kbit/s up, 2 Mbit/s down,
    /// 600 ms round trip, 5% loss.  An estimate, not a measurement: the
    /// slow uplink and the long, lossy round trip are what such links share.
    Train,
    /// Firefox DevTools' "Regular 3G" profile (devtools/client/shared/
    /// components/throttling/profiles.js): 250 kbit/s up, 750 down, 100 ms
    /// latency, here applied each way (200 ms round trip); no loss.
    ThreeG,
}

impl ThrottlePreset {
    pub const ALL: [ThrottlePreset; 3] = [ThrottlePreset::Airport, ThrottlePreset::Train, ThrottlePreset::ThreeG];

    pub fn as_str(self) -> &'static str {
        match self {
            ThrottlePreset::Airport => "airport",
            ThrottlePreset::Train => "train",
            ThrottlePreset::ThreeG => "3g",
        }
    }

    pub fn parse(name: &str) -> Option<ThrottlePreset> {
        Self::ALL.into_iter().find(|p| p.as_str().eq_ignore_ascii_case(name.trim()))
    }

    pub fn throttle(self) -> Throttle {
        match self {
            ThrottlePreset::Airport => Throttle { up_bits_per_sec: Some(1_000_000), down_bits_per_sec: Some(3_000_000), latency_ms: 200, loss_pct: 3 },
            ThrottlePreset::Train => Throttle { up_bits_per_sec: Some(512_000), down_bits_per_sec: Some(2_000_000), latency_ms: 300, loss_pct: 5 },
            ThrottlePreset::ThreeG => Throttle { up_bits_per_sec: Some(250_000), down_bits_per_sec: Some(750_000), latency_ms: 100, loss_pct: 0 },
        }
    }
}

/// Longest spec accepted: every key with a long value fits several times.
const SPEC_BYTES_MAX: usize = 256;
/// Most items in a spec: a preset and each of the four keys once.
const SPEC_ITEMS_MAX: usize = 5;
/// Slowest rate accepted, 1 kbit/s: below it a single request takes minutes.
const RATE_BITS_PER_SEC_MIN: u64 = 1_000;
/// Fastest rate accepted, 100 Gbit/s: past any real link.
const RATE_BITS_PER_SEC_MAX: u64 = 100_000_000_000;
/// Longest round trip accepted: past it, the harness's own timeouts fire
/// before anything is learned.
const RTT_MS_MAX: u64 = 60_000;
/// Fractional digits read in a number (`1.5mbit`, `0.25s`).
const FRACTION_DIGITS_MAX: u32 = 3;

impl Throttle {
    /// Parse a preset, a spec, or a preset followed by overrides.  Anything
    /// malformed is an error: a typo must never mean "no throttle".
    pub fn parse(spec: &str) -> anyhow::Result<Throttle> {
        let spec = spec.trim();
        anyhow::ensure!(!spec.is_empty(), "the throttle profile is empty");
        anyhow::ensure!(spec.len() <= SPEC_BYTES_MAX, "the throttle profile is longer than {SPEC_BYTES_MAX} bytes");
        let items: Vec<&str> = spec.split(',').map(str::trim).collect();
        anyhow::ensure!(items.len() <= SPEC_ITEMS_MAX, "the throttle profile has more than {SPEC_ITEMS_MAX} items");
        let mut throttle = Throttle { up_bits_per_sec: None, down_bits_per_sec: None, latency_ms: 0, loss_pct: 0 };
        let mut seen: Vec<&str> = Vec::with_capacity(SPEC_ITEMS_MAX);
        for (index, item) in items.iter().enumerate() {
            let Some((key, value)) = item.split_once('=') else {
                anyhow::ensure!(index == 0, "{item:?}: a preset comes first, then key=value overrides");
                throttle = ThrottlePreset::parse(item).ok_or_else(|| unknown_preset(item))?.throttle();
                continue;
            };
            let key = key.trim();
            anyhow::ensure!(!seen.contains(&key), "{key:?} is given twice");
            seen.push(key);
            throttle.apply(key, value.trim())?;
        }
        // Each key was checked as it was read; together they must hold too.
        throttle.validate().expect("a parsed profile is within the limits");
        Ok(throttle)
    }

    fn apply(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        match key {
            "up" => self.up_bits_per_sec = Some(parse_rate(value)?),
            "down" => self.down_bits_per_sec = Some(parse_rate(value)?),
            "rtt" => {
                let rtt_ms = parse_duration_ms(value)?;
                self.latency_ms = u32::try_from(rtt_ms / 2).expect("a round trip is at most RTT_MS_MAX");
            }
            "loss" => self.loss_pct = parse_loss_pct(value)?,
            other => anyhow::bail!("{other:?} is not a throttle key: use up, down, rtt or loss"),
        }
        Ok(())
    }

    /// Whether the profile is within the limits a spec can name.  The fields
    /// are public, so a profile built by hand is checked when the proxy starts.
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.loss_pct <= 100, "throttle loss {}% is over 100%", self.loss_pct);
        anyhow::ensure!(u64::from(self.latency_ms) * 2 <= RTT_MS_MAX, "throttle round trip {} ms is over {RTT_MS_MAX} ms", u64::from(self.latency_ms) * 2);
        for rate in [self.up_bits_per_sec, self.down_bits_per_sec].into_iter().flatten() {
            anyhow::ensure!((RATE_BITS_PER_SEC_MIN..=RATE_BITS_PER_SEC_MAX).contains(&rate), "throttle rate {rate} bit/s is outside 1 kbit/s to 100 Gbit/s");
        }
        Ok(())
    }
}

/// `1 Mbit/s up, 3 Mbit/s down, 400 ms RTT, 3% loss`.
impl fmt::Display for Throttle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rate = |bits: Option<u64>| bits.map_or_else(|| "unlimited".to_string(), format_rate);
        write!(f, "{} up, {} down, {} ms RTT, {}% loss", rate(self.up_bits_per_sec), rate(self.down_bits_per_sec), u64::from(self.latency_ms) * 2, self.loss_pct)
    }
}

fn unknown_preset(name: &str) -> anyhow::Error {
    let names: Vec<&str> = ThrottlePreset::ALL.iter().map(|p| p.as_str()).collect();
    anyhow::anyhow!("{name:?} is not a throttle preset: use {}, or a spec like up=1mbit,down=5mbit,rtt=300ms,loss=2", names.join(", "))
}

fn format_rate(bits_per_sec: u64) -> String {
    let (scale, unit) = if bits_per_sec >= 1_000_000 { (1_000_000, "Mbit/s") } else { (1_000, "kbit/s") };
    let whole = bits_per_sec / scale;
    let tenths = bits_per_sec % scale * 10 / scale;
    if tenths == 0 { format!("{whole} {unit}") } else { format!("{whole}.{tenths} {unit}") }
}

/// `512kbit`, `1mbit`, `1.5mbit`, in bits per second.
fn parse_rate(value: &str) -> anyhow::Result<u64> {
    let lower = value.to_ascii_lowercase();
    let (number, scale) = if let Some(n) = lower.strip_suffix("kbit") {
        (n, 1_000)
    } else if let Some(n) = lower.strip_suffix("mbit") {
        (n, 1_000_000)
    } else if let Some(n) = lower.strip_suffix("gbit") {
        (n, 1_000_000_000)
    } else {
        anyhow::bail!("{value:?}: a rate needs a unit, kbit, mbit or gbit (per second)");
    };
    let bits_per_sec = parse_scaled(number, scale).ok_or_else(|| anyhow::anyhow!("{value:?} is not a rate"))?;
    anyhow::ensure!(
        (RATE_BITS_PER_SEC_MIN..=RATE_BITS_PER_SEC_MAX).contains(&bits_per_sec),
        "{value:?}: a rate is between 1kbit and 100gbit"
    );
    Ok(bits_per_sec)
}

/// `300ms`, `1s`, `1.5s`, in milliseconds.
fn parse_duration_ms(value: &str) -> anyhow::Result<u64> {
    let lower = value.to_ascii_lowercase();
    let (number, scale) = if let Some(n) = lower.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = lower.strip_suffix('s') {
        (n, 1_000)
    } else {
        anyhow::bail!("{value:?}: a round trip needs a unit, ms or s");
    };
    let ms = parse_scaled(number, scale).ok_or_else(|| anyhow::anyhow!("{value:?} is not a duration"))?;
    anyhow::ensure!(ms <= RTT_MS_MAX, "{value:?}: a round trip is at most {}s", RTT_MS_MAX / 1_000);
    Ok(ms)
}

/// `2` or `2%`, a whole percentage from 0 to 100.
fn parse_loss_pct(value: &str) -> anyhow::Result<u8> {
    let number = value.strip_suffix('%').unwrap_or(value);
    let is_digits = !number.is_empty() && number.len() <= 3 && number.bytes().all(|b| b.is_ascii_digit());
    anyhow::ensure!(is_digits, "{value:?}: loss is a whole percentage, 0 to 100");
    let pct: u8 = number.parse().map_err(|_| anyhow::anyhow!("{value:?}: loss is a whole percentage, 0 to 100"))?;
    anyhow::ensure!(pct <= 100, "{value:?}: loss is at most 100");
    Ok(pct)
}

/// A decimal (`1`, `1.5`, `0.25`) times `scale`, exactly: digits only, at
/// most [`FRACTION_DIGITS_MAX`] after the point, no sign, no exponent.
fn parse_scaled(number: &str, scale: u64) -> Option<u64> {
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let is_number = !whole.is_empty() && whole.bytes().all(|b| b.is_ascii_digit()) && fraction.bytes().all(|b| b.is_ascii_digit());
    if !is_number || fraction.len() > FRACTION_DIGITS_MAX as usize || number.ends_with('.') {
        return None;
    }
    let whole: u64 = whole.parse().ok()?;
    let fraction_value: u64 = if fraction.is_empty() { 0 } else { fraction.parse().ok()? };
    let fraction_scale = 10u64.pow(u32::try_from(fraction.len()).expect("at most FRACTION_DIGITS_MAX digits"));
    let scaled = whole.checked_mul(scale)?.checked_add(fraction_value.checked_mul(scale)? / fraction_scale)?;
    Some(scaled)
}

/// Which requests are lost: SplitMix64 over a counter, so a seed replays the
/// same losses in the same order (tests), and the OS's seed varies them.
pub(crate) struct Loss {
    state: AtomicU64,
    loss_pct: u8,
}

impl Loss {
    pub(crate) fn new(seed: u64, loss_pct: u8) -> Loss {
        assert!(loss_pct <= 100);
        Loss { state: AtomicU64::new(seed), loss_pct }
    }

    /// Whether the next request is lost.
    pub(crate) fn is_lost(&self) -> bool {
        if self.loss_pct == 0 {
            return false;
        }
        if self.loss_pct == 100 {
            return true;
        }
        let roll = splitmix64(self.state.fetch_add(SPLITMIX_GAMMA, Ordering::Relaxed).wrapping_add(SPLITMIX_GAMMA));
        roll % 100 < u64::from(self.loss_pct)
    }
}

const SPLITMIX_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

fn splitmix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Bytes released at once by a paced body, most: a frame this size
/// stays smooth at any rate a profile allows.
const PACE_CHUNK_BYTES_MAX: usize = 16 * 1024;
/// Bytes released at once, least, so a slow rate doesn't mean a frame per byte.
const PACE_CHUNK_BYTES_MIN: usize = 256;
/// Each chunk holds about this much transmission time.
const PACE_CHUNK_MS: u64 = 20;

/// A body slowed to a rate as it streams: its data split into bounded
/// chunks, each released when a link of that rate would have finished
/// sending it.  The link is serial: a chunk starts when the previous one
/// is done, or when it arrives, whichever is later, so an idle stream
/// earns no burst.  Without a rate, frames pass straight through.
pub(crate) struct Paced<B> {
    inner: B,
    rate: Option<Rate>,
}

struct Rate {
    bits_per_sec: u64,
    chunk_bytes: usize,
    /// Data received and not yet released.
    pending: Bytes,
    /// A chunk waiting for `sleep`.
    held: Option<Bytes>,
    sleep: Option<Pin<Box<Sleep>>>,
    /// When the link finishes sending what was released so far.
    link_free_at: Option<Instant>,
}

impl<B> Paced<B> {
    pub(crate) fn new(inner: B, bits_per_sec: Option<u64>) -> Paced<B> {
        let rate = bits_per_sec.map(|bits_per_sec| {
            assert!(bits_per_sec > 0, "a rate is positive");
            let bytes_per_chunk_time = bits_per_sec / 8 * PACE_CHUNK_MS / 1_000;
            let chunk_bytes = usize::try_from(bytes_per_chunk_time).unwrap_or(usize::MAX).clamp(PACE_CHUNK_BYTES_MIN, PACE_CHUNK_BYTES_MAX);
            Rate { bits_per_sec, chunk_bytes, pending: Bytes::new(), held: None, sleep: None, link_free_at: None }
        });
        Paced { inner, rate }
    }
}

impl Rate {
    /// The next chunk of `pending`, once the link has sent it.
    fn poll_release(&mut self, cx: &mut Context<'_>) -> Poll<Bytes> {
        if self.held.is_none() {
            debug_assert!(!self.pending.is_empty());
            let length = self.pending.len().min(self.chunk_bytes);
            let chunk = self.pending.split_to(length);
            let now = Instant::now();
            let starts_at = self.link_free_at.map_or(now, |free| free.max(now));
            let done_at = starts_at + self.transmission(chunk.len());
            self.link_free_at = Some(done_at);
            self.held = Some(chunk);
            self.sleep = Some(Box::pin(tokio::time::sleep_until(done_at.into())));
        }
        let sleep = self.sleep.as_mut().expect("a held chunk has its sleep");
        if sleep.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        self.sleep = None;
        let chunk = self.held.take().expect("a held chunk is released once");
        assert!(chunk.len() <= self.chunk_bytes);
        Poll::Ready(chunk)
    }

    fn transmission(&self, bytes: usize) -> Duration {
        let bits = u128::from(bytes as u64) * 8;
        let nanos = bits * 1_000_000_000 / u128::from(self.bits_per_sec);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    fn has_data(&self) -> bool {
        self.held.is_some() || !self.pending.is_empty()
    }

    fn buffered_bytes(&self) -> u64 {
        (self.pending.len() + self.held.as_ref().map_or(0, Bytes::len)) as u64
    }
}

impl<B: Body<Data = Bytes> + Unpin> Body for Paced<B> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = self.get_mut();
        let Some(rate) = this.rate.as_mut() else {
            return Pin::new(&mut this.inner).poll_frame(cx);
        };
        if rate.has_data() {
            return rate.poll_release(cx).map(|chunk| Some(Ok(Frame::data(chunk))));
        }
        let frame = match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(frame) => frame,
        };
        match frame {
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) if data.is_empty() => Poll::Ready(Some(Ok(Frame::data(data)))),
                Ok(data) => {
                    rate.pending = data;
                    rate.poll_release(cx).map(|chunk| Some(Ok(Frame::data(chunk))))
                }
                // Trailers come after the data, so nothing is held back.
                Err(frame) => Poll::Ready(Some(Ok(frame))),
            },
            other => Poll::Ready(other),
        }
    }

    fn is_end_stream(&self) -> bool {
        let has_data = self.rate.as_ref().is_some_and(Rate::has_data);
        !has_data && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let inner = self.inner.size_hint();
        let buffered_bytes = self.rate.as_ref().map_or(0, Rate::buffered_bytes);
        let mut hint = SizeHint::new();
        hint.set_lower(inner.lower().saturating_add(buffered_bytes));
        if let Some(upper) = inner.upper() {
            hint.set_upper(upper.saturating_add(buffered_bytes));
        }
        hint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_parse_by_name_and_take_overrides() {
        for preset in ThrottlePreset::ALL {
            assert_eq!(Throttle::parse(preset.as_str()).unwrap(), preset.throttle());
        }
        assert_eq!(Throttle::parse(" Airport ").unwrap(), ThrottlePreset::Airport.throttle());
        let airport = Throttle::parse("airport, loss=0, rtt=1s").unwrap();
        assert_eq!((airport.loss_pct, airport.latency_ms, airport.up_bits_per_sec), (0, 500, Some(1_000_000)));
        assert_eq!(ThrottlePreset::Airport.throttle().to_string(), "1 Mbit/s up, 3 Mbit/s down, 400 ms RTT, 3% loss");
        assert_eq!(ThrottlePreset::ThreeG.throttle().to_string(), "250 kbit/s up, 750 kbit/s down, 200 ms RTT, 0% loss");
    }

    #[test]
    fn a_spec_reads_its_units() {
        let t = Throttle::parse("up=1mbit,down=5mbit,rtt=300ms,loss=2").unwrap();
        assert_eq!(t, Throttle { up_bits_per_sec: Some(1_000_000), down_bits_per_sec: Some(5_000_000), latency_ms: 150, loss_pct: 2 });
        let t = Throttle::parse("down=512kbit,rtt=1.5s,loss=100%").unwrap();
        assert_eq!(t, Throttle { up_bits_per_sec: None, down_bits_per_sec: Some(512_000), latency_ms: 750, loss_pct: 100 });
        assert_eq!(Throttle::parse("up=1.5MBIT").unwrap().up_bits_per_sec, Some(1_500_000));
        assert_eq!(Throttle::parse("up=2gbit").unwrap().up_bits_per_sec, Some(2_000_000_000));
        assert_eq!(Throttle::parse("rtt=0ms").unwrap().latency_ms, 0);
        assert_eq!(Throttle::parse("up=1.5mbit").unwrap().to_string(), "1.5 Mbit/s up, unlimited down, 0 ms RTT, 0% loss");
    }

    #[test]
    fn a_malformed_spec_is_an_error() {
        let long = format!("up={}kbit", "1".repeat(SPEC_BYTES_MAX));
        for bad in [
            "", " ", "motorway", "up=1", "up=1mb", "up=mbit", "up=-1mbit", "up=1e3kbit", "up=1.mbit", "up=1.2345mbit", "up=0kbit", "up=0.5kbit",
            "up=200gbit", "rtt=300", "rtt=61s", "rtt=-1ms", "loss=101", "loss=2.5", "loss=", "loss=-1", "speed=1mbit", "up=1mbit,up=2mbit",
            "up=1mbit,airport", "airport,train", "up=1mbit,", ",", "airport,,loss=1", "up=99999999999999999999kbit", &long,
            "up=1kbit,down=1kbit,rtt=1ms,loss=1,up=2kbit,down=2kbit",
        ] {
            assert!(Throttle::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn losses_are_replayed_by_their_seed() {
        let draw = |seed: u64, pct: u8| {
            let loss = Loss::new(seed, pct);
            (0..1_000).map(|_| loss.is_lost()).collect::<Vec<bool>>()
        };
        assert_eq!(draw(7, 30), draw(7, 30), "the same seed loses the same requests");
        assert_ne!(draw(7, 30), draw(8, 30));
        let lost = draw(7, 30).iter().filter(|l| **l).count();
        assert!((250..350).contains(&lost), "about 30% of 1000: {lost}");
        assert!(draw(1, 0).iter().all(|l| !l));
        assert!(draw(1, 100).iter().all(|l| *l));
    }
}
