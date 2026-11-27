//! Ladybird on a pseudo-terminal of our own, between it and the pane
//! (RFD 0007).
//!
//! Terminal mode keeps its terminal's last row for a status line (the
//! page's address and port, its keys) and has no option to hide it.  So
//! Ladybird gets a pty one row taller than the pane: the page fills every
//! row of the pane, and what Ladybird writes to the extra row is dropped.
//! Its frames are opaque, so the page matches the pane by taking the
//! terminal's own colours instead (`theme`).
//!
//! The pane's size is read here too.  Ladybird takes a pixel ratio only when
//! it starts, so when the pane's cells change size enough to change it (the
//! pane shown on a phone, then on the Mac again), Ladybird is started again
//! at the new ratio.  A terminal that reports no pixel size is asked for it
//! (`CSI 14 t`); without an answer the page draws at ratio 1, as before.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// How often the pane's size is looked at, and how long a read waits.
const TICK_MS: i32 = 200;
/// How long the terminal has to answer what it is asked at the start.  A
/// terminal answers in a few milliseconds; one that never answers the last
/// question (`CSI c`, which every terminal answers) costs this much.
const ANSWERS_MS: u64 = 400;
/// The most of the terminal's answers read: 19 short replies.
const ANSWERS_BYTES_MAX: usize = 8 * 1024;
/// The most one read moves.  A Kitty frame is written in many of these.
const CHUNK_BYTES: usize = 64 * 1024;
/// The longest escape sequence the filter holds back.  The status line's
/// are a few bytes; anything longer passes through as it is.
const SEQUENCE_BYTES_MAX: usize = 64;
/// The most times Ladybird is started again for a new pixel ratio.  Each is
/// a switch between screens; past this the pane keeps the ratio it has.
const RESTARTS_MAX: u32 = 64;

/// The process group of the Ladybird running now, or 0: what
/// [`end_running`] ends when the pane hangs up.
static RUNNING_GROUP: AtomicI32 = AtomicI32::new(0);

/// End the Ladybird running now, and its helpers.  For the pane's hangup:
/// its pty stays open in us until we exit, so it would not hear one itself.
pub fn end_running() {
    let group = RUNNING_GROUP.swap(0, Ordering::Relaxed);
    if group > 0 {
        end_group(group);
    }
}

/// An RGB colour, 8 bits a channel.
pub type Rgb = (u8, u8, u8);

/// The pane terminal's colours, as far as it answered: its text and
/// background (OSC 10 and 11) and its 16-colour palette (OSC 4), which the
/// terminal view draws agents and dim text in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Colors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub palette: [Option<Rgb>; 16],
}

/// The pane's size in cells and device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneSize {
    pub rows: u16,
    pub cols: u16,
    pub width_px: u16,
    pub height_px: u16,
}

impl PaneSize {
    /// Device pixels per cell row, when the size has pixels.
    pub fn cell_height_px(&self) -> Option<f64> {
        (self.rows > 0 && self.height_px > 0).then(|| f64::from(self.height_px) / f64::from(self.rows))
    }

    /// What Ladybird is told: one row more, and one cell's pixels more, so
    /// the rows above its status line are exactly the pane's.
    fn for_ladybird(&self) -> libc::winsize {
        assert!(self.rows > 0, "a pane has rows");
        let cell = self.height_px / self.rows;
        libc::winsize {
            ws_row: self.rows.saturating_add(1),
            ws_col: self.cols,
            ws_xpixel: self.width_px,
            ws_ypixel: self.height_px.saturating_add(cell),
        }
    }

    /// The same cells at a new count: pixels per cell stay when a pane is
    /// only resized, which is what a terminal that reports no pixels needs.
    fn with_cells_of(&self, rows: u16, cols: u16) -> PaneSize {
        let scale = |px: u16, from: u16, to: u16| if from == 0 { 0 } else { u16::try_from(u32::from(px) * u32::from(to) / u32::from(from)).unwrap_or(u16::MAX) };
        PaneSize { rows, cols, width_px: scale(self.width_px, self.cols, cols), height_px: scale(self.height_px, self.rows, rows) }
    }
}

/// Run `browser` with `flags`, `--device-pixel-ratio` from the pane's size
/// and `url`, until it exits or the pane hangs up.  `ratio_for` turns a
/// pane size into the ratio to draw at.  `on_start` hears the pane, the
/// ratio and the terminal's colours before each start, so the page can be
/// fitted to them.  Without a terminal there is no status line to hide or
/// size to follow: Ladybird runs as it is, at the ratio of a pane of
/// unknown size.
pub fn run(
    browser: &Path,
    flags: &[&str],
    url: &str,
    ratio_for: impl Fn(&PaneSize) -> f64,
    mut on_start: impl FnMut(&PaneSize, f64, &Colors),
) -> Result<ExitStatus> {
    let stdin = std::io::stdin().as_raw_fd();
    // SAFETY: isatty only reads the descriptor's type.
    if unsafe { libc::isatty(stdin) } != 1 {
        let unknown = PaneSize { rows: 0, cols: 0, width_px: 0, height_px: 0 };
        let ratio = ratio_for(&unknown);
        on_start(&unknown, ratio, &Colors::default());
        let mut command = Command::new(browser);
        command.args(flags).args(["--device-pixel-ratio", &ratio.to_string(), url]);
        return command.status().with_context(|| format!("starting {}", browser.display()));
    }
    let _raw = RawMode::enter(stdin).context("putting the pane's terminal in raw mode")?;
    let mut size = pane_size(stdin).context("the pane has no size")?;
    let (pixels, colors) = ask_terminal(stdin, size.cell_height_px().is_none());
    if let Some((height_px, width_px)) = pixels {
        size = PaneSize { width_px, height_px, ..size };
    }
    let hung_up = Arc::new(AtomicBool::new(false));
    let stop_input = Arc::new(AtomicBool::new(false));
    // Where typing goes: the pty of the Ladybird running now.
    let typing_to: Arc<Mutex<Option<File>>> = Arc::new(Mutex::new(None));
    let input = forward_input(stdin, typing_to.clone(), hung_up.clone(), stop_input.clone());
    let mut restarts = 0_u32;
    let status = loop {
        let ratio = ratio_for(&size);
        on_start(&size, ratio, &colors);
        // A fresh pty each time: the last one may still be the controlling
        // terminal of a helper of the Ladybird before.
        let (master, slave) = open_pty(&size.for_ladybird())?;
        let mut child = spawn(browser, flags, url, ratio, &slave)?;
        drop(slave);
        RUNNING_GROUP.store(libc::pid_t::try_from(child.id()).expect("a pid fits pid_t"), Ordering::Relaxed);
        *typing_to.lock().map_err(|_| anyhow::anyhow!("the input thread panicked"))? = Some(File::from(master.try_clone()?));
        let mut output = File::from(master);
        let fd = output.as_raw_fd();
        let ended = match pump(&mut child, &mut output, fd, &mut size, &hung_up, |s| ratio_for(s) != ratio) {
            Ok(ended) => ended,
            Err(e) => {
                end(&mut child);
                return Err(e);
            }
        };
        match ended {
            Ended::Exited(status) => break status,
            Ended::HungUp => break end(&mut child),
            Ended::NewRatio if restarts < RESTARTS_MAX => {
                restarts += 1;
                end(&mut child);
            }
            Ended::NewRatio => break child.wait()?,
        }
    };
    RUNNING_GROUP.store(0, Ordering::Relaxed);
    stop_input.store(true, Ordering::Relaxed);
    // The input thread notices within a tick; a panic there lost only input.
    let _ = input.join();
    Ok(status)
}

/// Why [`pump`] returned.
enum Ended {
    Exited(ExitStatus),
    HungUp,
    /// The pane changed size enough to want another pixel ratio.
    NewRatio,
}

/// Copy Ladybird's output to the pane without its status line, and follow
/// the pane's size, until something in [`Ended`] happens.
fn pump(
    child: &mut Child,
    output: &mut File,
    master: RawFd,
    size: &mut PaneSize,
    hung_up: &AtomicBool,
    wants_new_ratio: impl Fn(&PaneSize) -> bool,
) -> Result<Ended> {
    let stdin = std::io::stdin().as_raw_fd();
    let mut filter = StatusLineFilter::new(size.rows.saturating_add(1));
    let mut chunk = vec![0_u8; CHUNK_BYTES];
    let mut shown = Vec::with_capacity(CHUNK_BYTES);
    let mut stdout = std::io::stdout().lock();
    loop {
        if hung_up.load(Ordering::Relaxed) {
            return Ok(Ended::HungUp);
        }
        if let Some(status) = child.try_wait()? {
            return Ok(Ended::Exited(status));
        }
        if readable(master, TICK_MS)? {
            match output.read(&mut chunk) {
                Ok(0) => return Ok(Ended::Exited(child.wait()?)),
                Ok(n) => {
                    shown.clear();
                    filter.feed(&chunk[..n], &mut shown);
                    // A pane that can't be written to is gone.
                    if stdout.write_all(&shown).and_then(|()| stdout.flush()).is_err() {
                        return Ok(Ended::HungUp);
                    }
                }
                // EIO: every writer of the pty is gone, so Ladybird is.
                Err(_) => return Ok(Ended::Exited(child.wait()?)),
            }
        }
        let Some(now) = pane_size(stdin) else { continue };
        if (now.rows, now.cols, now.width_px, now.height_px) == (size.rows, size.cols, size.width_px, size.height_px) {
            continue;
        }
        let now = if now.cell_height_px().is_none() { size.with_cells_of(now.rows, now.cols) } else { now };
        *size = now;
        set_size(master, &now.for_ladybird())?;
        filter.status_row = now.rows.saturating_add(1);
        if wants_new_ratio(&now) {
            return Ok(Ended::NewRatio);
        }
    }
}

/// How long Ladybird has to quit on SIGTERM before it is killed.
const QUIT_GRACE_MS: u64 = 1_500;

/// End Ladybird and every helper it started (its session's process group).
fn end(child: &mut Child) -> ExitStatus {
    end_group(libc::pid_t::try_from(child.id()).expect("a pid fits pid_t"));
    // A child that can't be waited for was reaped already: it is gone.
    child.wait().unwrap_or_else(|_| ExitStatus::from_raw(libc::SIGKILL))
}

/// SIGTERM to the process group `group`, then SIGKILL once the grace is up.
fn end_group(group: libc::pid_t) {
    assert!(group > 0, "a process group, never every process");
    // SAFETY: signalling a process group we started; a group already gone
    // returns ESRCH, which is the outcome we want.
    unsafe { libc::kill(-group, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_millis(QUIT_GRACE_MS);
    // SAFETY: signal 0 only asks whether the group still has a process.
    while Instant::now() < deadline && unsafe { libc::kill(-group, 0) } == 0 {
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: as above; past the grace period it is killed outright.
    unsafe { libc::kill(-group, libc::SIGKILL) };
}

/// Start Ladybird on the pty, as the leader of a session it controls.
fn spawn(browser: &Path, flags: &[&str], url: &str, ratio: f64, slave: &OwnedFd) -> Result<Child> {
    let mut command = Command::new(browser);
    command
        .args(flags)
        .args(["--device-pixel-ratio", &ratio.to_string(), url])
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: setsid and ioctl are async-signal-safe; they make the pty this
    // child's controlling terminal, as a shell's is.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().with_context(|| format!("starting {}", browser.display()))
}

/// Copy what the pane types to Ladybird, without Ctrl-L: it opens the
/// address prompt on the status line, which isn't shown.  Sets `hung_up`
/// when the pane goes; stops when `stop` is set.
fn forward_input(stdin: RawFd, to: Arc<Mutex<Option<File>>>, hung_up: Arc<AtomicBool>, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // SAFETY: stdin stays open for the life of the process; ManuallyDrop
        // keeps this File from closing it.
        let mut from = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(stdin) });
        let mut chunk = vec![0_u8; CHUNK_BYTES];
        while !stop.load(Ordering::Relaxed) {
            match readable(stdin, TICK_MS) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(_) => break,
            }
            let n = match from.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let Ok(mut to) = to.lock() else { break };
            // Typing while Ladybird restarts lands in the pty it is starting on,
            // or, written to one it left, is lost with it.
            if let Some(pty) = to.as_mut() {
                let _ = pty.write_all(&without_address_key(&chunk[..n]));
            }
        }
        if !stop.load(Ordering::Relaxed) {
            hung_up.store(true, Ordering::Relaxed);
        }
    })
}

/// `bytes` without Ctrl-L, as a plain byte or as a Kitty keyboard report
/// (`CSI 108;5u`, with an event type or not).  A terminal writes one key
/// report per write, so a report split across two reads isn't looked for.
fn without_address_key(bytes: &[u8]) -> Vec<u8> {
    const REPORTS: [&[u8]; 4] = [b"\x1b[108;5u", b"\x1b[108;5:1u", b"\x1b[108;5:2u", b"\x1b[108;5:3u"];
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(report) = REPORTS.iter().find(|r| bytes[i..].starts_with(r)) {
            i += report.len();
        } else if bytes[i] == 0x0c {
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    debug_assert!(out.len() <= bytes.len());
    out
}

/// Drops what Ladybird writes to its status line: from a cursor move to
/// that row up to the next cursor move elsewhere or the next image.
/// Everything else passes through unchanged and in order.
struct StatusLineFilter {
    /// The 1-based row Ladybird's status line is on.
    status_row: u16,
    dropping: bool,
    /// An escape sequence seen in part, held until it is whole.
    held: Vec<u8>,
}

impl StatusLineFilter {
    fn new(status_row: u16) -> StatusLineFilter {
        assert!(status_row > 0, "rows count from 1");
        StatusLineFilter { status_row, dropping: false, held: Vec::with_capacity(SEQUENCE_BYTES_MAX) }
    }

    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &byte in bytes {
            if self.held.is_empty() {
                if byte == 0x1b {
                    self.held.push(byte);
                } else if !self.dropping {
                    out.push(byte);
                }
                continue;
            }
            self.held.push(byte);
            self.settle(out);
        }
    }

    /// Decide on the held sequence once it is whole, or can't be one we act on.
    fn settle(&mut self, out: &mut Vec<u8>) {
        let held = &self.held;
        assert!(held.len() >= 2 && held[0] == 0x1b, "a held sequence starts with ESC");
        if held[1] != b'[' {
            // Not a CSI: an image (`ESC _`) starts after the status line is done.
            if held[1] == b'_' {
                self.dropping = false;
            }
            self.release(out);
            return;
        }
        let last = *held.last().expect("held is not empty");
        let whole = held.len() > 2 && (0x40..=0x7e).contains(&last);
        if !whole && held.len() < SEQUENCE_BYTES_MAX {
            return;
        }
        if whole && last == b'H' {
            let row = std::str::from_utf8(&held[2..held.len() - 1]).ok().and_then(|p| p.split(';').next()).and_then(|r| r.parse::<u16>().ok());
            self.dropping = row == Some(self.status_row);
        }
        self.release(out);
    }

    fn release(&mut self, out: &mut Vec<u8>) {
        if !self.dropping {
            out.extend_from_slice(&self.held);
        }
        self.held.clear();
    }
}

/// Puts a terminal in raw mode, and back as it was when dropped.
struct RawMode {
    fd: RawFd,
    saved: libc::termios,
}

impl RawMode {
    fn enter(fd: RawFd) -> Result<RawMode> {
        // SAFETY: tcgetattr fills the struct it is given; a zeroed termios is
        // a valid value to be overwritten.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            bail!("not a terminal: {}", std::io::Error::last_os_error());
        }
        let mut raw = saved;
        // SAFETY: cfmakeraw only edits the struct; tcsetattr reads it.
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            bail!("raw mode: {}", std::io::Error::last_os_error());
        }
        Ok(RawMode { fd, saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring the settings read in `enter`.  A pane already
        // gone can't be restored, and needs no restoring.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

fn pane_size(fd: RawFd) -> Option<PaneSize> {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ fills the struct we pass.
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_row > 0 && size.ws_col > 0).then_some(PaneSize { rows: size.ws_row, cols: size.ws_col, width_px: size.ws_xpixel, height_px: size.ws_ypixel })
}

fn set_size(fd: RawFd, size: &libc::winsize) -> Result<()> {
    // SAFETY: TIOCSWINSZ reads the struct we pass; the kernel then signals
    // the pty's foreground group (Ladybird) with SIGWINCH.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, size) } != 0 {
        bail!("resizing Ladybird's terminal: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Ask the terminal, in one round trip, for its pixel size when `pixels`
/// (`CSI 14 t`, answered `CSI 4 ; height ; width t`), its text and
/// background colours, and its 16-colour palette.  `CSI c` goes last:
/// every terminal answers it, so its answer means the others are in.
fn ask_terminal(fd: RawFd, pixels: bool) -> (Option<(u16, u16)>, Colors) {
    let mut question = String::from(if pixels { "\x1b[14t" } else { "" });
    question.push_str("\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    for index in 0..16 {
        question.push_str(&format!("\x1b]4;{index};?\x1b\\"));
    }
    question.push_str("\x1b[c");
    let mut stdout = std::io::stdout().lock();
    if stdout.write_all(question.as_bytes()).and_then(|()| stdout.flush()).is_err() {
        return (None, Colors::default());
    }
    // SAFETY: as in `forward_input`: stdin stays open, and isn't closed here.
    let mut from = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    let deadline = Instant::now() + Duration::from_millis(ANSWERS_MS);
    let mut answers = Vec::with_capacity(ANSWERS_BYTES_MAX);
    let mut chunk = [0_u8; 1024];
    while Instant::now() < deadline && answers.len() < ANSWERS_BYTES_MAX && !has_device_attributes(&answers) {
        match readable(fd, 20) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => break,
        }
        match from.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => answers.extend_from_slice(&chunk[..n]),
        }
    }
    (parse_pixels_reply(&answers), parse_colors(&answers))
}

/// Whether `answers` hold the answer to `CSI c`: `CSI ? … c`.
fn has_device_attributes(answers: &[u8]) -> bool {
    answers.windows(3).enumerate().any(|(at, w)| {
        w == b"\x1b[?" && answers[at + 3..].iter().find(|b| !(b.is_ascii_digit() || **b == b';')) == Some(&b'c')
    })
}

/// The colours in `answers`: OSC 10, 11 and 4 replies, ended by BEL or ST.
fn parse_colors(answers: &[u8]) -> Colors {
    let mut colors = Colors::default();
    let text = String::from_utf8_lossy(answers);
    for reply in text.split("\x1b]").skip(1) {
        let body = reply.split(['\x07', '\x1b']).next().unwrap_or("");
        let mut parts = body.split(';');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("10"), Some(spec), None) => colors.foreground = parse_color(spec),
            (Some("11"), Some(spec), None) => colors.background = parse_color(spec),
            (Some("4"), Some(index), Some(spec)) => {
                if let Some(slot) = index.parse::<usize>().ok().and_then(|i| colors.palette.get_mut(i)) {
                    *slot = parse_color(spec);
                }
            }
            _ => {}
        }
    }
    colors
}

/// `rgb:R/G/B`, each one to four hex digits, as 8 bits a channel.
fn parse_color(spec: &str) -> Option<Rgb> {
    let channels: Vec<u8> = spec
        .strip_prefix("rgb:")?
        .split('/')
        .map(|c| {
            let max = 16_u32.checked_pow(u32::try_from(c.len()).ok()?)?.checked_sub(1)?;
            let value = u32::from_str_radix(c, 16).ok()?;
            (1..=4).contains(&c.len()).then(|| u8::try_from(value * 255 / max).expect("a scaled channel is a byte"))
        })
        .collect::<Option<_>>()?;
    match channels[..] {
        [r, g, b] => Some((r, g, b)),
        _ => None,
    }
}

/// `(height, width)` from `CSI 4 ; height ; width t`.
fn parse_pixels_reply(reply: &[u8]) -> Option<(u16, u16)> {
    let start = reply.windows(4).position(|w| w == b"\x1b[4;")?;
    let body = std::str::from_utf8(&reply[start + 4..reply.len().checked_sub(1)?]).ok()?;
    let (height, width) = body.split_once(';')?;
    let (height, width) = (height.parse::<u16>().ok()?, width.parse::<u16>().ok()?);
    (height > 0 && width > 0).then_some((height, width))
}

fn open_pty(size: &libc::winsize) -> Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    let mut size = *size;
    // SAFETY: openpty writes two descriptors into the integers given and
    // reads the size we pass.
    let opened = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
    if opened != 0 {
        bail!("opening a terminal for Ladybird: {}", std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just opened and are owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // Neither may leak into Ladybird beyond the stdio it is given: with the
    // master open in it, its terminal would never hang up when we go.
    for fd in [&master, &slave] {
        // SAFETY: setting a flag on a descriptor owned here.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            bail!("opening a terminal for Ladybird: {}", std::io::Error::last_os_error());
        }
    }
    Ok((master, slave))
}

/// Whether `fd` has something to read within `timeout_ms`.
fn readable(fd: RawFd, timeout_ms: i32) -> std::io::Result<bool> {
    let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: one pollfd, owned here, for the duration of the call.
    let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::Interrupted { Ok(false) } else { Err(error) };
    }
    // A hang-up reads as readable: the read that follows says it is over.
    Ok(ready > 0 && poll.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered(status_row: u16, chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = StatusLineFilter::new(status_row);
        let mut out = Vec::new();
        for chunk in chunks {
            filter.feed(chunk, &mut out);
        }
        out
    }

    /// What Ladybird 2026.47.7 writes at start, as captured from a pty.
    const START: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[?1003h\x1b[20;1H\x1b[2K\x1b[32m\xe2\x97\x8f\x1b[0m\x1b[2m about:blank \xe2\x80\x94 ctrl-l url \x1b[0m\x1b[20;1H\x1b[2K\x1b[33m\xe2\x97\x8f\x1b[0m\x1b[2m http://127.0.0.1:50123/\x1b[0m\x1b[1;1H\x1b_Ga=T,f=24;AAAA\x1b\\";

    #[test]
    fn the_status_line_is_dropped_and_everything_else_kept() {
        let kept = b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[?1003h\x1b[1;1H\x1b_Ga=T,f=24;AAAA\x1b\\";
        assert_eq!(filtered(20, &[START]), kept);
        // Split anywhere, the same.
        for at in 1..START.len() {
            assert_eq!(filtered(20, &[&START[..at], &START[at..]]), kept, "split at {at}");
        }
        // Another row is the page's: kept.
        assert_eq!(filtered(21, &[b"\x1b[20;1Hok"]), b"\x1b[20;1Hok");
        // A plain cursor move ends the drop.
        assert_eq!(filtered(20, &[b"\x1b[20;1Hgone\x1b[5;3Hkept"]), b"\x1b[5;3Hkept");
    }

    #[test]
    fn a_long_sequence_passes_through_whole() {
        let long = [b"\x1b[".as_slice(), &[b'1'; 100], b"m"].concat();
        assert_eq!(filtered(20, &[&long]), long);
    }

    #[test]
    fn ctrl_l_never_reaches_ladybird() {
        assert_eq!(without_address_key(b"a\x0cb"), b"ab");
        assert_eq!(without_address_key(b"x\x1b[108;5uy\x1b[108;5:3u"), b"xy");
        assert_eq!(without_address_key(b"\x1b[114;5u"), b"\x1b[114;5u", "Ctrl-R reloads, and goes through");
    }

    #[test]
    fn ladybird_gets_one_row_more_with_the_pane_exactly_above_it() {
        let pane = PaneSize { rows: 40, cols: 100, width_px: 1600, height_px: 1360 };
        let told = pane.for_ladybird();
        assert_eq!((told.ws_row, told.ws_col, told.ws_xpixel, told.ws_ypixel), (41, 100, 1600, 1394));
        assert_eq!(pane.cell_height_px(), Some(34.0));
        let resized = pane.with_cells_of(20, 50);
        assert_eq!((resized.width_px, resized.height_px), (800, 680), "a resize keeps the cell size");
        assert_eq!(PaneSize { height_px: 0, ..pane }.cell_height_px(), None);
    }

    #[test]
    fn the_terminal_says_its_colours() {
        let answers = b"\x1b]10;rgb:ecec/ecec/f1f1\x1b\\\x1b]11;rgb:1d/1d/21\x07\x1b]4;3;rgb:e6e6/b4b4/5050\x1b\\\x1b]4;99;rgb:0/0/0\x1b\\\x1b[?62;22c";
        let colors = parse_colors(answers);
        assert_eq!(colors.foreground, Some((0xec, 0xec, 0xf1)));
        assert_eq!(colors.background, Some((0x1d, 0x1d, 0x21)), "two hex digits, ended by BEL");
        assert_eq!(colors.palette[3], Some((0xe6, 0xb4, 0x50)));
        assert_eq!(colors.palette.iter().flatten().count(), 1, "an index past 15 is no palette entry");
        assert!(has_device_attributes(answers));
        assert!(!has_device_attributes(b"\x1b]11;rgb:0/0/0\x07"));
        assert_eq!(parse_color("rgb:f/8/0"), Some((255, 136, 0)));
        assert_eq!(parse_color("rgb:12345/0/0"), None);
        assert_eq!(parse_color("#ffffff"), None);
    }

    #[test]
    fn the_terminal_says_its_pixel_size() {
        assert_eq!(parse_pixels_reply(b"\x1b[4;1360;1600t"), Some((1360, 1600)));
        assert_eq!(parse_pixels_reply(b"junk\x1b[4;680;800t"), Some((680, 800)));
        assert_eq!(parse_pixels_reply(b"\x1b[4;0;0t"), None);
        assert_eq!(parse_pixels_reply(b"\x1b[8;24;80t"), None);
    }
}
