// local diagnostics: a few counts and coarse timings that stay on this computer,
// so a user who hits a freeze or a crash can look at them and decide whether to
// share them. nothing here opens a network connection or sends anything.
//
// what is recorded is typed: counters, fixed step names and coarse durations.
// no reminder text, path, name, id, exact time or panic message is ever stored,
// so none can be exported. the file on disk is not trusted either: it is read
// with a size cap, and the report is rebuilt from its numbers and enums.
//
// limits, stated plainly:
// - an exit is "unclean" when a launch never recorded a normal quit. that covers
//   a crash, a force quit, a power loss and a failed final write. on macOS, a
//   quit the system asks for with terminate: (a logout, restart, shutdown, or a
//   Quit Apple Event) is recorded as normal by watch_terminate, if the record is
//   written in time. the cause is unknown, and a native crash is never counted
//   as a confirmed crash.
// - a panic is a panic observation. a panic on a worker thread does not end the
//   app. the count is written within about a second, so a panic that kills the
//   process in that window can be missed.
// - a stall is a main thread that did not answer a probe for 5 seconds. the
//   probe runs through the same event loop as the menu, so an open menu that
//   holds the loop is counted if it holds it that long.
// - one process records at a time (a file lock). a second copy of Nudge shows
//   the records and says it is not recording.

use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::*};
use std::sync::{Once, OnceLock};
use std::thread::Thread;
use std::time::{Duration, Instant, SystemTime};

const SCHEMA: u32 = 1;
const MAX_SESSIONS: usize = 20;
const MAX_EVENTS: usize = 96;
// the journal is a few kilobytes; anything near this size is not ours
const MAX_JOURNAL_BYTES: u64 = 128 * 1024;
const MAX_BUILD_CHARS: usize = 24;
const MAX_BUILDS_LISTED: usize = 8;

const JOURNAL: &str = "journal.json";
const JOURNAL_TMP: &str = "journal.json.tmp";
const JOURNAL_BAD: &str = "journal.bad";
const LEASE: &str = "lease.lock";
const OFF_MARKER: &str = "recording-off";

// a main thread that has not answered for this long counts as stalled
const STALL_MS: u64 = 5_000;
// after a sleep, a scheduler pause or a clock jump, probes wait this long
const GRACE_MS: u64 = 12_000;
// the worker thread's own gap between beats may be this far over a second
// before it is read as a pause
const BEAT_SLACK_MS: u64 = 3_000;
// a step must take this long to be counted at all
const SLOW_EDGES_MS: [u64; 4] = [100, 500, 1_000, 5_000];
const BUCKETS: usize = SLOW_EDGES_MS.len();
const BUCKET_LABELS: [&str; BUCKETS] = ["0.1 to 0.5 s", "0.5 to 1 s", "1 to 5 s", "5 s or more"];
// the shortest gap between writes that only change the timing counters
const SAVE_EVERY_MS: u64 = 30_000;
// how long quitting waits for the final record
const CLEAN_EXIT_WAIT: Duration = Duration::from_millis(300);

// ----- steps -----

// what the main thread is doing, named in advance; the only way a step enters a record
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    Idle,
    Startup,
    TrayRefresh,
    TrayConfig,
    TraySchedule,
    TrayMenu,
    TrayIcon,
    SettingsOpen,
    OverlayOpen,
    OverlayClose,
    ConfigSave,
    Quit,
    // not a main thread step: a panic on another thread
    Background,
    #[serde(other)]
    Unknown,
}

// the steps that can be timed
const PHASE_COUNT: usize = 12;
const SLOTS: usize = PHASE_COUNT * BUCKETS;
const BACKGROUND: u8 = Phase::Background as u8;

impl Phase {
    fn from_u8(value: u8) -> Phase {
        match value {
            0 => Phase::Idle,
            1 => Phase::Startup,
            2 => Phase::TrayRefresh,
            3 => Phase::TrayConfig,
            4 => Phase::TraySchedule,
            5 => Phase::TrayMenu,
            6 => Phase::TrayIcon,
            7 => Phase::SettingsOpen,
            8 => Phase::OverlayOpen,
            9 => Phase::OverlayClose,
            10 => Phase::ConfigSave,
            11 => Phase::Quit,
            BACKGROUND => Phase::Background,
            _ => Phase::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Startup => "starting up",
            Phase::TrayRefresh => "refreshing the tray",
            Phase::TrayConfig => "refreshing the tray, waiting for settings",
            Phase::TraySchedule => "refreshing the tray, waiting for the schedule",
            Phase::TrayMenu => "updating the tray menu",
            Phase::TrayIcon => "updating the tray icon",
            Phase::SettingsOpen => "opening Settings",
            Phase::OverlayOpen => "opening a reminder",
            Phase::OverlayClose => "closing a reminder",
            Phase::ConfigSave => "saving settings",
            Phase::Quit => "quitting",
            Phase::Background => "on a background thread",
            Phase::Unknown => "unknown",
        }
    }
}

fn bucket(took: Duration) -> Option<usize> {
    let ms = took.as_millis() as u64;
    SLOW_EDGES_MS.iter().rposition(|&edge| ms >= edge)
}

fn slot(phase: Phase, bucket: usize) -> Option<usize> {
    let index = phase as usize;
    (index < PHASE_COUNT && bucket < BUCKETS).then_some(index * BUCKETS + bucket)
}

// ----- what the main thread touches -----

// everything a hook on the main thread or in the panic hook reads or writes is
// an atomic here: no lock, no allocation, no disk
struct Counters {
    phase: AtomicU8,
    panics: AtomicU32,
    panic_phase: AtomicU8,
    slow: [AtomicU32; SLOTS],
    // the last probe the main thread answered
    pong: AtomicU64,
    // whether the preference lets anything be recorded
    enabled: AtomicBool,
    // an off choice that could not be saved: stay off until quit
    forced_off: AtomicBool,
    status: AtomicU8,
    problem: AtomicU8,
    clean_requested: AtomicBool,
    clean_ack: AtomicU8,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU32 = AtomicU32::new(0);

impl Counters {
    const fn new() -> Self {
        Counters {
            phase: AtomicU8::new(0),
            panics: AtomicU32::new(0),
            panic_phase: AtomicU8::new(BACKGROUND),
            slow: [ZERO; SLOTS],
            pong: AtomicU64::new(0),
            enabled: AtomicBool::new(true),
            forced_off: AtomicBool::new(false),
            status: AtomicU8::new(Status::Recording as u8),
            problem: AtomicU8::new(0),
            clean_requested: AtomicBool::new(false),
            clean_ack: AtomicU8::new(0),
        }
    }

    fn slow_now(&self) -> [u32; SLOTS] {
        let mut out = [0; SLOTS];
        for (value, counter) in out.iter_mut().zip(&self.slow) {
            *value = counter.load(Relaxed);
        }
        out
    }
}

fn bump(counter: &AtomicU32) {
    let _ = counter.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_add(1)));
}

static COUNTERS: Counters = Counters::new();
static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static WORKER: OnceLock<Thread> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

thread_local! {
    static MAIN: Cell<bool> = const { Cell::new(false) };
}

fn on_main() -> bool {
    MAIN.try_with(Cell::get).unwrap_or(false)
}

// times one step on the main thread and names it while it runs; on any other
// thread it does nothing. a step that takes 0.1 second or more is counted
pub struct Span {
    phase: Phase,
    previous: u8,
    start: Instant,
    main: bool,
}

pub fn span(phase: Phase) -> Span {
    let main = on_main();
    let previous = if main {
        COUNTERS.phase.swap(phase as u8, Relaxed)
    } else {
        0
    };
    Span {
        phase,
        previous,
        start: Instant::now(),
        main,
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if !self.main {
            return;
        }
        COUNTERS.phase.store(self.previous, Relaxed);
        if COUNTERS.enabled.load(Relaxed) {
            if let Some(index) = bucket(self.start.elapsed()).and_then(|b| slot(self.phase, b)) {
                bump(&COUNTERS.slow[index]);
            }
        }
    }
}

// ----- status -----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Status {
    Recording,
    Off,
    PrefUnreadable,
    InUse,
    Full,
    Unwritable,
}

impl Status {
    fn from_u8(value: u8) -> Status {
        match value {
            0 => Status::Recording,
            1 => Status::Off,
            2 => Status::PrefUnreadable,
            3 => Status::InUse,
            4 => Status::Full,
            _ => Status::Unwritable,
        }
    }

    // fixed codes; the page turns them into words
    fn code(self) -> &'static str {
        match self {
            Status::Recording => "recording",
            Status::Off => "off",
            Status::PrefUnreadable => "setting_unreadable",
            Status::InUse => "in_use",
            Status::Full => "disk_full",
            Status::Unwritable => "unwritable",
        }
    }

    fn of_error(error: &io::Error) -> Status {
        // ENOSPC on Unix, ERROR_DISK_FULL and ERROR_HANDLE_DISK_FULL on Windows
        let full = if cfg!(windows) {
            matches!(error.raw_os_error(), Some(112 | 39))
        } else {
            error.raw_os_error() == Some(28)
        };
        if full {
            Status::Full
        } else {
            Status::Unwritable
        }
    }
}

const PROBLEM_NONE: u8 = 0;
const PROBLEM_STOP_UNSAVED: u8 = 1;
const PROBLEM_START_UNSAVED: u8 = 2;

// ----- the record -----

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
enum End {
    Running,
    Clean,
    // a later launch found it still marked Running with nobody holding the lock
    Lost,
    // recording was turned off during this launch; also what an unrecognised value reads as
    #[default]
    #[serde(other)]
    Stopped,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
enum Kind {
    Stall,
    Recovered,
    Panic,
    Lost,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
struct Event {
    // the launch it belongs to, shown only as how far back it is
    s: u32,
    kind: Kind,
    phase: Phase,
    // whole seconds in steps of 5
    secs: u32,
}

impl Default for Event {
    fn default() -> Self {
        Event {
            s: 0,
            kind: Kind::Other,
            phase: Phase::Unknown,
            secs: 0,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Session {
    n: u32,
    build: String,
    end: End,
    panics: u32,
    stalls: u32,
    slow: Vec<u32>,
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Journal {
    v: u32,
    next: u32,
    sessions: Vec<Session>,
    events: Vec<Event>,
}

// a version or platform string from a file: kept only if it looks like one
fn clean_build(text: &str) -> String {
    let plain = !text.is_empty()
        && text.chars().count() <= MAX_BUILD_CHARS
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    if plain {
        text.to_string()
    } else {
        "unknown".into()
    }
}

impl Journal {
    fn fresh() -> Self {
        Journal {
            v: SCHEMA,
            next: 1,
            sessions: Vec::new(),
            events: Vec::new(),
        }
    }

    // hold anything read from disk, or grown in memory, to the limits
    fn sanitize(&mut self) {
        if self.sessions.len() > MAX_SESSIONS {
            self.sessions.drain(..self.sessions.len() - MAX_SESSIONS);
        }
        for session in &mut self.sessions {
            session.build = clean_build(&session.build);
            session.slow.resize(SLOTS, 0);
        }
        let top = self.sessions.iter().map(|s| s.n).max().unwrap_or(0);
        self.next = self.next.max(top.saturating_add(1)).max(1);
        // events about launches that have been dropped go with them
        if let Some(oldest) = self.sessions.iter().map(|s| s.n).min() {
            self.events.retain(|e| e.s >= oldest);
        }
        if self.events.len() > MAX_EVENTS {
            self.events.drain(..self.events.len() - MAX_EVENTS);
        }
    }

    fn event(&mut self, kind: Kind, phase: Phase, secs: u32) {
        let s = self.sessions.last().map_or(0, |s| s.n);
        self.events.push(Event {
            s,
            kind,
            phase,
            secs: secs / 5 * 5,
        });
        self.sanitize();
    }
}

#[derive(Debug, PartialEq)]
enum Load {
    Missing,
    Ok,
    // unreadable, too big, not JSON, or another format: not trusted
    Bad,
}

fn load_journal(dir: &Path) -> (Journal, Load) {
    let file = match File::open(dir.join(JOURNAL)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (Journal::fresh(), Load::Missing)
        }
        Err(_) => return (Journal::fresh(), Load::Bad),
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_JOURNAL_BYTES
    {
        return (Journal::fresh(), Load::Bad);
    }
    match serde_json::from_slice::<Journal>(&bytes) {
        Ok(mut journal) if journal.v == SCHEMA => {
            journal.sanitize();
            (journal, Load::Ok)
        }
        _ => (Journal::fresh(), Load::Bad),
    }
}

// write beside the journal, then rename over it, so a crash part way leaves
// the old record whole
fn write_journal(dir: &Path, journal: &Journal) -> io::Result<()> {
    let bytes = serde_json::to_vec(journal).map_err(io::Error::other)?;
    let temp = dir.join(JOURNAL_TMP);
    let result = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(JOURNAL))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

// ----- the preference -----

// the choice lives in a file of its own, apart from config.json: it has to be
// readable when settings are not, and a second copy of Nudge can change it
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pref {
    On,
    Off,
    // present but unreadable is not the same as absent: that stays off
    Unreadable,
}

fn read_pref(dir: &Path) -> Pref {
    match fs::symlink_metadata(dir.join(OFF_MARKER)) {
        Ok(_) => Pref::Off,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Pref::On,
        Err(_) => Pref::Unreadable,
    }
}

fn write_pref(dir: &Path, on: bool) -> io::Result<()> {
    let marker = dir.join(OFF_MARKER);
    if on {
        match fs::remove_file(&marker) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    } else {
        fs::create_dir_all(dir)?;
        fs::write(marker, b"off\n")
    }
}

// ----- the lock that makes one process the writer -----

struct Lease(#[allow(dead_code)] File);

enum LeaseError {
    Busy,
    Io(io::Error),
}

#[cfg(unix)]
fn lock(file: &File) -> Result<(), LeaseError> {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    const LOCK_EX_NB: i32 = 2 | 4;
    // SAFETY: the descriptor belongs to the open file, which outlives the call
    if unsafe { flock(file.as_raw_fd(), LOCK_EX_NB) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    Err(if error.kind() == io::ErrorKind::WouldBlock {
        LeaseError::Busy
    } else {
        LeaseError::Io(error)
    })
}

// on Windows the exclusive open is the lock
#[cfg(windows)]
fn lock(_: &File) -> Result<(), LeaseError> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn lock(_: &File) -> Result<(), LeaseError> {
    Err(LeaseError::Io(io::Error::from(io::ErrorKind::Unsupported)))
}

impl Lease {
    // the file is never deleted, so every process locks the same one; the
    // system drops the lock when the holder dies
    fn acquire(dir: &Path) -> Result<Lease, LeaseError> {
        fs::create_dir_all(dir).map_err(LeaseError::Io)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let file = match options.open(dir.join(LEASE)) {
            Ok(file) => file,
            // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
            Err(error) if cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33)) => {
                return Err(LeaseError::Busy)
            }
            Err(error) => return Err(LeaseError::Io(error)),
        };
        lock(&file)?;
        Ok(Lease(file))
    }
}

// ----- noticing a frozen main thread -----

#[derive(Clone, Copy, Debug)]
struct Clock {
    // milliseconds that do not move while the computer sleeps (on macOS)
    mono: u64,
    // milliseconds of wall clock
    wall: u64,
}

#[derive(Debug, PartialEq)]
enum Seen {
    Nothing,
    Stalled(Phase),
    Recovered { phase: Phase, secs: u32 },
}

// posts one probe to the main thread at a time and notices when it is not
// answered. it knows nothing about Tauri, so the timing can be tested
#[derive(Default)]
struct Watch {
    last: Option<Clock>,
    seq: u64,
    pending: Option<(u64, u64)>,
    grace_until: u64,
    stalled: Option<Phase>,
}

impl Watch {
    // forget any probe in flight, so turning recording off and on again
    // cannot read an old silence as a freeze
    fn reset(&mut self) {
        self.last = None;
        self.pending = None;
        self.stalled = None;
    }

    // returns the probe to post, if any, and what the answer (or its absence) says
    fn step(&mut self, now: Clock, answered: u64, phase: Phase) -> (Option<u64>, Seen) {
        if let Some(last) = self.last {
            let beat = now.mono.saturating_sub(last.mono);
            let wall = now.wall as i64 - last.wall as i64;
            // this thread was not running (a scheduler pause), or the computer
            // slept or the clock moved: whatever the main thread did, it was not a freeze
            if beat > 1_000 + BEAT_SLACK_MS || (wall - beat as i64).unsigned_abs() > BEAT_SLACK_MS {
                self.grace_until = now.mono + GRACE_MS;
                self.pending = None;
                self.stalled = None;
            }
        }
        self.last = Some(now);
        if now.mono < self.grace_until {
            return (None, Seen::Nothing);
        }

        let mut seen = Seen::Nothing;
        if let Some((seq, since)) = self.pending {
            if answered >= seq {
                self.pending = None;
                if let Some(phase) = self.stalled.take() {
                    seen = Seen::Recovered {
                        phase,
                        secs: (now.mono.saturating_sub(since) / 1_000) as u32,
                    };
                }
            } else if now.mono.saturating_sub(since) >= STALL_MS && self.stalled.is_none() {
                self.stalled = Some(phase);
                seen = Seen::Stalled(phase);
            }
        }
        let post = if self.pending.is_none() {
            self.seq += 1;
            self.pending = Some((self.seq, now.mono));
            Some(self.seq)
        } else {
            None
        };
        (post, seen)
    }
}

// ----- the one process that writes -----

struct Recorder {
    dir: PathBuf,
    counters: &'static Counters,
    build: &'static str,
    lease: Option<Lease>,
    journal: Journal,
    live: bool,
    dirty: bool,
    base_panics: u32,
    seen_panics: u32,
    base_slow: [u32; SLOTS],
    saved_slow: [u32; SLOTS],
    saved_at: u64,
    watch: Watch,
}

impl Recorder {
    fn new(dir: PathBuf, counters: &'static Counters, build: &'static str) -> Self {
        Recorder {
            dir,
            counters,
            build,
            lease: None,
            journal: Journal::fresh(),
            live: false,
            dirty: false,
            base_panics: 0,
            seen_panics: 0,
            base_slow: [0; SLOTS],
            saved_slow: [0; SLOTS],
            saved_at: 0,
            watch: Watch::default(),
        }
    }

    fn set_status(&self, status: Status) {
        self.counters.status.store(status as u8, Relaxed);
    }

    fn pref(&self) -> Pref {
        if self.counters.forced_off.load(Relaxed) {
            Pref::Off
        } else {
            read_pref(&self.dir)
        }
    }

    // one beat: follow the preference, record what happened, and say which
    // probe to send the main thread
    fn tick(&mut self, now: Clock) -> Option<u64> {
        let pref = self.pref();
        self.counters.enabled.store(pref == Pref::On, Relaxed);
        if pref != Pref::On {
            self.stop();
            self.set_status(if pref == Pref::Unreadable {
                Status::PrefUnreadable
            } else {
                Status::Off
            });
            return None;
        }
        if !self.live && !self.begin() {
            return None;
        }

        let panics = self.counters.panics.load(Relaxed);
        if panics > self.seen_panics {
            self.seen_panics = panics;
            let phase = Phase::from_u8(self.counters.panic_phase.load(Relaxed));
            self.journal.event(Kind::Panic, phase, 0);
            self.dirty = true;
        }

        let phase = Phase::from_u8(self.counters.phase.load(Relaxed));
        let (post, seen) = self
            .watch
            .step(now, self.counters.pong.load(Acquire), phase);
        match seen {
            Seen::Nothing => {}
            Seen::Stalled(phase) => {
                if let Some(session) = self.journal.sessions.last_mut() {
                    session.stalls = session.stalls.saturating_add(1);
                }
                self.journal
                    .event(Kind::Stall, phase, (STALL_MS / 1_000) as u32);
                self.dirty = true;
            }
            Seen::Recovered { phase, secs } => {
                self.journal.event(Kind::Recovered, phase, secs);
                self.dirty = true;
            }
        }
        let timing_changed = self.counters.slow_now() != self.saved_slow_total();
        if self.dirty || (timing_changed && now.mono.saturating_sub(self.saved_at) >= SAVE_EVERY_MS)
        {
            self.persist(now.mono);
        }
        post
    }

    // the timings as the last write left them, in the counters' own terms
    fn saved_slow_total(&self) -> [u32; SLOTS] {
        let mut out = self.saved_slow;
        for (value, base) in out.iter_mut().zip(&self.base_slow) {
            *value = value.saturating_add(*base);
        }
        out
    }

    fn sync_session(&mut self) {
        let now = self.counters.slow_now();
        let panics = self
            .counters
            .panics
            .load(Relaxed)
            .saturating_sub(self.base_panics);
        if let Some(session) = self.journal.sessions.last_mut() {
            session.panics = panics;
            for (index, value) in session.slow.iter_mut().enumerate() {
                *value = now[index].saturating_sub(self.base_slow[index]);
            }
        }
    }

    fn fail(&self, error: LeaseError) {
        self.set_status(match error {
            LeaseError::Busy => Status::InUse,
            LeaseError::Io(error) => Status::of_error(&error),
        });
    }

    // a launch starts: take the lock, settle what earlier launches left
    // behind, and add this one. false while it cannot be done
    fn begin(&mut self) -> bool {
        if self.lease.is_none() {
            match Lease::acquire(&self.dir) {
                Ok(lease) => self.lease = Some(lease),
                Err(error) => {
                    self.fail(error);
                    return false;
                }
            }
        }
        let (mut journal, load) = load_journal(&self.dir);
        if load == Load::Bad {
            let _ = fs::rename(self.dir.join(JOURNAL), self.dir.join(JOURNAL_BAD));
        }
        // holding the lock means no other launch is alive, so one still marked
        // Running ended without a recorded quit
        let lost: Vec<u32> = journal
            .sessions
            .iter_mut()
            .filter(|s| s.end == End::Running)
            .map(|s| {
                s.end = End::Lost;
                s.n
            })
            .collect();
        for n in lost {
            journal.events.push(Event {
                s: n,
                kind: Kind::Lost,
                phase: Phase::Unknown,
                secs: 0,
            });
        }
        journal.sessions.push(Session {
            n: journal.next,
            build: clean_build(self.build),
            end: End::Running,
            slow: vec![0; SLOTS],
            ..Session::default()
        });
        journal.next = journal.next.saturating_add(1);
        journal.sanitize();
        if let Err(error) = write_journal(&self.dir, &journal) {
            self.set_status(Status::of_error(&error));
            return false;
        }
        self.journal = journal;
        self.live = true;
        self.dirty = false;
        self.base_panics = self.counters.panics.load(Relaxed);
        self.seen_panics = self.base_panics;
        self.base_slow = self.counters.slow_now();
        self.saved_slow = [0; SLOTS];
        self.watch.reset();
        self.set_status(Status::Recording);
        true
    }

    // write the record, unless the preference changed since the last beat
    fn persist(&mut self, mono: u64) {
        if self.pref() != Pref::On {
            self.stop();
            return;
        }
        self.sync_session();
        match write_journal(&self.dir, &self.journal) {
            Ok(()) => {
                self.dirty = false;
                self.saved_at = mono;
                self.saved_slow = self.journal.sessions.last().map_or([0; SLOTS], |s| {
                    let mut out = [0; SLOTS];
                    out.iter_mut().zip(&s.slow).for_each(|(o, v)| *o = *v);
                    out
                });
                self.set_status(Status::Recording);
            }
            // kept in memory and tried again on the next beat
            Err(error) => self.set_status(Status::of_error(&error)),
        }
    }

    // recording stops: this launch is closed as Stopped with one last write,
    // which saves the timings gathered since the last save, and the lock is let go
    // so another copy can take over
    fn stop(&mut self) {
        self.watch.reset();
        if self.live {
            self.live = false;
            self.sync_session();
            if let Some(session) = self.journal.sessions.last_mut() {
                session.end = End::Stopped;
            }
            let _ = write_journal(&self.dir, &self.journal);
        }
        self.lease = None;
    }

    // the app is returning from its event loop: close this launch as clean
    fn finish_clean(&mut self) -> io::Result<()> {
        if !self.live {
            return Ok(());
        }
        if self.pref() != Pref::On {
            self.stop();
            return Ok(());
        }
        self.sync_session();
        if let Some(session) = self.journal.sessions.last_mut() {
            session.end = End::Clean;
        }
        write_journal(&self.dir, &self.journal)?;
        self.live = false;
        Ok(())
    }
}

// ----- the report -----

struct Totals {
    panics: u64,
    stalls: u64,
    unclean: u64,
    launches: usize,
}

fn totals(journal: &Journal) -> Totals {
    Totals {
        panics: journal.sessions.iter().map(|s| u64::from(s.panics)).sum(),
        stalls: journal.sessions.iter().map(|s| u64::from(s.stalls)).sum(),
        unclean: journal
            .sessions
            .iter()
            .filter(|s| s.end == End::Lost)
            .count() as u64,
        launches: journal.sessions.len(),
    }
}

// the text a person reviews and may copy. built only from numbers and fixed
// labels, so nothing in the file can reach it as text
fn render_report(journal: &Journal, recording: bool, build: &str) -> String {
    let t = totals(journal);
    let mut out = String::new();
    let _ = writeln!(out, "Nudge diagnostics report (format {SCHEMA})");
    let _ = writeln!(out, "Nudge version: {}", clean_build(build));
    let _ = writeln!(
        out,
        "System: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let _ = writeln!(
        out,
        "Recording now: {}",
        if recording { "yes" } else { "no" }
    );
    let _ = writeln!(
        out,
        "Launches in this report: {} (at most {MAX_SESSIONS} are kept)",
        t.launches
    );
    let mut builds: Vec<&str> = Vec::new();
    for session in &journal.sessions {
        if !builds.contains(&session.build.as_str()) && builds.len() < MAX_BUILDS_LISTED {
            builds.push(&session.build);
        }
    }
    let _ = writeln!(
        out,
        "Versions: {}",
        if builds.is_empty() {
            "none".into()
        } else {
            builds.join(", ")
        }
    );
    out.push('\n');
    let _ = writeln!(out, "Panics: {}", t.panics);
    let _ = writeln!(
        out,
        "UI stalls (the app did not respond for 5 seconds or more): {}",
        t.stalls
    );
    let _ = writeln!(
        out,
        "Unclean exits (no normal quit was recorded, cause unknown): {}",
        t.unclean
    );
    out.push('\n');

    let _ = writeln!(out, "Slow steps (times a step took 0.1 second or longer)");
    let mut any = false;
    for index in 0..PHASE_COUNT {
        let parts: Vec<String> = (0..BUCKETS)
            .filter_map(|b| {
                let n: u64 = journal
                    .sessions
                    .iter()
                    .map(|s| u64::from(s.slow.get(index * BUCKETS + b).copied().unwrap_or(0)))
                    .sum();
                (n > 0).then(|| format!("{} x{n}", BUCKET_LABELS[b]))
            })
            .collect();
        if !parts.is_empty() {
            any = true;
            let _ = writeln!(
                out,
                "- {}: {}",
                Phase::from_u8(index as u8).label(),
                parts.join("; ")
            );
        }
    }
    if !any {
        out.push_str("- none recorded\n");
    }
    out.push('\n');

    let _ = writeln!(out, "Recent events, oldest first");
    let latest = journal.sessions.last().map_or(0, |s| s.n);
    if journal.events.is_empty() {
        out.push_str("- none recorded\n");
    }
    for event in &journal.events {
        let ago = match latest.saturating_sub(event.s) {
            0 => "latest launch".to_string(),
            1 => "1 launch earlier".to_string(),
            n => format!("{n} launches earlier"),
        };
        let what = match event.kind {
            Kind::Stall => format!("UI stall (step: {})", event.phase.label()),
            Kind::Recovered => format!(
                "UI recovered after at least {} s (stall began in: {})",
                event.secs,
                event.phase.label()
            ),
            Kind::Panic => format!("panic (step: {})", event.phase.label()),
            Kind::Lost => "launch ended without a recorded quit".to_string(),
            Kind::Other => "unrecognised event".to_string(),
        };
        let _ = writeln!(out, "- {ago}: {what}");
    }
    out.push('\n');
    out.push_str("Made on this computer from counts and fixed labels only. It has no reminder text, names, paths, times or device identifiers. Nudge has not sent it anywhere.\n");
    out
}

// ----- the page's side -----

#[derive(Serialize)]
pub struct View {
    // the choice, as far as this copy of Nudge can tell
    keep: bool,
    status: &'static str,
    // a choice that did not save, for the page to say so
    problem: Option<&'static str>,
    panics: u64,
    stalls: u64,
    unclean: u64,
    launches: usize,
}

fn dir() -> Option<&'static Path> {
    DIR.get().and_then(|d| d.as_deref())
}

fn recording_now() -> bool {
    COUNTERS.enabled.load(Relaxed)
        && Status::from_u8(COUNTERS.status.load(Relaxed)) == Status::Recording
}

fn make_view(journal: &Journal, counters: &Counters) -> View {
    let t = totals(journal);
    View {
        keep: counters.enabled.load(Relaxed),
        status: Status::from_u8(counters.status.load(Relaxed)).code(),
        problem: match counters.problem.load(Relaxed) {
            PROBLEM_STOP_UNSAVED => Some("stop_unsaved"),
            PROBLEM_START_UNSAVED => Some("start_unsaved"),
            _ => None,
        },
        panics: t.panics,
        stalls: t.stalls,
        unclean: t.unclean,
        launches: t.launches,
    }
}

fn view() -> View {
    let journal = dir()
        .map(|d| load_journal(d).0)
        .unwrap_or_else(Journal::fresh);
    make_view(&journal, &COUNTERS)
}

fn report() -> String {
    let journal = dir()
        .map(|d| load_journal(d).0)
        .unwrap_or_else(Journal::fresh);
    render_report(&journal, recording_now(), env!("CARGO_PKG_VERSION"))
}

// turn recording on or off. off takes effect here at once even when the choice
// cannot be saved, and then stays off until quit; on only counts once saved
fn apply_choice(dir: Option<&Path>, counters: &Counters, on: bool) {
    let saved = match dir {
        Some(dir) => write_pref(dir, on),
        None => Err(io::Error::from(io::ErrorKind::NotFound)),
    };
    match (on, saved) {
        (true, Ok(())) => {
            counters.forced_off.store(false, Relaxed);
            counters.problem.store(PROBLEM_NONE, Relaxed);
            counters.enabled.store(true, Relaxed);
        }
        (true, Err(_)) => counters.problem.store(PROBLEM_START_UNSAVED, Relaxed),
        (false, saved) => {
            counters.enabled.store(false, Relaxed);
            counters.status.store(Status::Off as u8, Relaxed);
            if saved.is_err() {
                counters.forced_off.store(true, Relaxed);
                counters.problem.store(PROBLEM_STOP_UNSAVED, Relaxed);
            } else {
                counters.problem.store(PROBLEM_NONE, Relaxed);
            }
        }
    }
}

fn set_recording(on: bool) {
    apply_choice(dir(), &COUNTERS, on);
    if let Some(worker) = WORKER.get() {
        worker.unpark();
    }
}

// async so the file reads run off the main thread
#[tauri::command]
pub async fn get_diagnostics() -> View {
    view()
}

#[tauri::command]
pub async fn set_diagnostics(enabled: bool) -> View {
    set_recording(enabled);
    view()
}

// the page shows exactly this text and copies exactly this text
#[tauri::command]
pub async fn review_diagnostics() -> String {
    report()
}

// ----- starting, watching and ending -----

fn clock() -> Clock {
    let start = *START.get_or_init(Instant::now);
    Clock {
        mono: start.elapsed().as_millis() as u64,
        wall: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
    }
}

// first thing in main, on the main thread: where records go, the panic hook,
// and the state of the preference before anything is timed
pub fn init(data_dir: Option<PathBuf>) {
    let _ = MAIN.try_with(|main| main.set(true));
    START.get_or_init(Instant::now);
    let dir = data_dir.map(|d| d.join("diagnostics"));
    if let Some(dir) = &dir {
        let pref = read_pref(dir);
        COUNTERS.enabled.store(pref == Pref::On, Relaxed);
        if pref != Pref::On {
            let status = if pref == Pref::Unreadable {
                Status::PrefUnreadable
            } else {
                Status::Off
            };
            COUNTERS.status.store(status as u8, Relaxed);
        }
    } else {
        COUNTERS.status.store(Status::Unwritable as u8, Relaxed);
    }
    let _ = DIR.set(dir);
    install_panic_hook();
}

// counts a panic without reading it: no payload, no message, no location, no
// lock. the usual hook still runs afterwards
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            observe_panic();
            previous(info);
        }));
    });
}

fn observe_panic() {
    if !COUNTERS.enabled.load(Relaxed) {
        return;
    }
    let phase = if on_main() {
        COUNTERS.phase.load(Relaxed)
    } else {
        BACKGROUND
    };
    COUNTERS.panic_phase.store(phase, Relaxed);
    bump(&COUNTERS.panics);
    if let Some(worker) = WORKER.get() {
        worker.unpark();
    }
}

// the end of the setup closure: start the one background thread
pub fn start(app: &tauri::AppHandle) {
    let Some(dir) = dir().map(Path::to_path_buf) else {
        return;
    };
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("nudge-diagnostics".into())
        .spawn(move || worker(handle, dir));
    match spawned {
        Ok(worker) => {
            let _ = WORKER.set(worker.thread().clone());
        }
        Err(_) => COUNTERS.status.store(Status::Unwritable as u8, Relaxed),
    }
    #[cfg(debug_assertions)]
    test_hook(app);
}

fn worker(app: tauri::AppHandle, dir: PathBuf) {
    let mut recorder = Recorder::new(dir, &COUNTERS, env!("CARGO_PKG_VERSION"));
    loop {
        if let Some(seq) = recorder.tick(clock()) {
            // answered by the main thread whenever it next runs events
            let _ = app.run_on_main_thread(move || COUNTERS.pong.store(seq, Release));
        }
        if COUNTERS.clean_requested.load(Acquire) {
            let ok = recorder.finish_clean().is_ok();
            COUNTERS.clean_ack.store(if ok { 1 } else { 2 }, Release);
            return;
        }
        std::thread::park_timeout(Duration::from_secs(1));
    }
}

#[derive(Debug, PartialEq)]
enum CleanExit {
    Recorded,
    Failed,
    TimedOut,
}

fn request_clean(counters: &Counters, worker: Option<&Thread>, wait: Duration) -> CleanExit {
    let Some(worker) = worker else {
        return CleanExit::Recorded;
    };
    counters.clean_requested.store(true, Release);
    worker.unpark();
    let deadline = Instant::now() + wait;
    loop {
        match counters.clean_ack.load(Acquire) {
            1 => return CleanExit::Recorded,
            2 => return CleanExit::Failed,
            _ if Instant::now() >= deadline => return CleanExit::TimedOut,
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

// after the event loop has returned and Tauri has cleaned up: the normal quit
// path. on macOS, watch_terminate calls it too. waits at most 300 ms; a record that could not be
// written leaves this launch to be counted as unclean, which is what it will look like
pub fn mark_clean_exit() {
    match request_clean(&COUNTERS, WORKER.get(), CLEAN_EXIT_WAIT) {
        CleanExit::Recorded => {}
        CleanExit::Failed => println!("diagnostics: the normal quit could not be recorded"),
        CleanExit::TimedOut => println!("diagnostics: the normal quit was not recorded in time"),
    }
}

// on macOS, a logout, restart, shutdown or a Quit from another app ends the app
// with terminate:, which never returns to run_return. AppKit posts
// NSApplicationWillTerminateNotification first, on the main thread, and it cannot
// be cancelled, so the normal quit is recorded there. a second record is harmless:
// once the worker has finished, request_clean returns at once.
#[cfg(target_os = "macos")]
pub fn watch_terminate() {
    use block2::RcBlock;
    use objc2_app_kit::NSApplicationWillTerminateNotification;
    use objc2_foundation::{NSNotification, NSNotificationCenter};
    use std::ptr::NonNull;

    let block = RcBlock::new(|_: NonNull<NSNotification>| mark_clean_exit());
    // SAFETY: a nil queue runs the block on the posting thread, which is the main
    // thread for terminate:. the block is copied by the notification center, so
    // dropping ours is fine. the observer lasts as long as the process, so its
    // token is kept by forgetting it rather than removed
    let token = unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSApplicationWillTerminateNotification),
            None,
            None,
            &block,
        )
    };
    std::mem::forget(token);
}

// debug builds only: NUDGE_DIAGNOSTICS_TEST=stall|panic|quit makes this copy
// misbehave once, a few seconds after recording is up, so the real path can be
// watched. release builds do not contain it
#[cfg(debug_assertions)]
fn test_hook(app: &tauri::AppHandle) {
    let Some(action) = std::env::var("NUDGE_DIAGNOSTICS_TEST")
        .ok()
        .filter(|a| matches!(a.as_str(), "stall" | "panic" | "quit"))
    else {
        return;
    };
    let app = app.clone();
    std::thread::spawn(move || {
        while Status::from_u8(COUNTERS.status.load(Relaxed)) != Status::Recording
            || WORKER.get().is_none()
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_secs(4));
        match action.as_str() {
            "stall" => {
                let _ = app.run_on_main_thread(|| {
                    let _step = span(Phase::TrayMenu);
                    std::thread::sleep(Duration::from_secs(8));
                });
            }
            "panic" => {
                // a payload that looks personal, to show it never reaches a record
                let _ = std::thread::spawn(|| {
                    panic!("test panic carrying /Users/sentinel/secret-reminder.txt");
                });
            }
            _ => app.exit(0),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // the process-wide counters and hook are shared by the tests that use them
    static GLOBAL: Mutex<()> = Mutex::new(());

    fn counters() -> &'static Counters {
        Box::leak(Box::new(Counters::new()))
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nudge-diag-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn at(mono: u64) -> Clock {
        Clock {
            mono,
            wall: 1_000_000_000 + mono,
        }
    }

    fn recorder(dir: &Path) -> Recorder {
        Recorder::new(dir.to_path_buf(), counters(), "1.2.3")
    }

    fn disk(dir: &Path) -> Journal {
        load_journal(dir).0
    }

    // beat once a second until `until`, answering every probe like a healthy main thread
    fn run_healthy(rec: &mut Recorder, from: u64, until: u64) {
        let mut t = from;
        while t <= until {
            if let Some(seq) = rec.tick(at(t)) {
                rec.counters.pong.store(seq, Release);
            }
            t += 1_000;
        }
    }

    #[test]
    fn a_frozen_main_thread_is_recorded_while_it_is_still_frozen_and_its_recovery_after() {
        let dir = temp("stall");
        let mut rec = recorder(&dir);
        assert_eq!(rec.tick(at(0)), Some(1), "the first beat sends a probe");
        rec.counters.phase.store(Phase::TrayMenu as u8, Relaxed);
        for t in [1_000, 2_000, 3_000, 4_000] {
            assert_eq!(rec.tick(at(t)), None, "one probe at a time");
        }
        assert_eq!(disk(&dir).sessions[0].stalls, 0, "4 seconds is not a stall");
        rec.tick(at(5_000));
        // the probe is still unanswered, and the stall is already on disk with its step
        let during = disk(&dir);
        assert_eq!(during.sessions[0].stalls, 1);
        assert_eq!(during.events.last().unwrap().kind, Kind::Stall);
        assert_eq!(during.events.last().unwrap().phase, Phase::TrayMenu);
        rec.tick(at(9_000));
        assert_eq!(disk(&dir).sessions[0].stalls, 1, "one freeze is one stall");
        rec.counters.pong.store(1, Release);
        assert_eq!(rec.tick(at(10_000)), Some(2));
        let after = disk(&dir);
        let last = after.events.last().unwrap();
        assert_eq!(
            (last.kind, last.phase, last.secs),
            (Kind::Recovered, Phase::TrayMenu, 10)
        );
        assert_eq!(after.sessions[0].stalls, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_healthy_main_thread_records_nothing_beyond_the_launch() {
        let dir = temp("healthy");
        let mut rec = recorder(&dir);
        run_healthy(&mut rec, 0, 120_000);
        let journal = disk(&dir);
        assert_eq!((journal.sessions.len(), journal.events.len()), (1, 0));
        assert_eq!(journal.sessions[0].end, End::Running);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sleep_or_a_paused_process_is_not_a_freeze() {
        // the computer sleeps for ten minutes: the wall clock jumps, the steady one does not
        let dir = temp("sleep");
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        rec.tick(at(1_000));
        let woke = Clock {
            mono: 2_000,
            wall: 1_000_000_000 + 2_000 + 600_000,
        };
        assert_eq!(rec.tick(woke), None);
        // still no answer to the probe sent before sleep, and no stall through the grace period
        for t in (3_000..=13_000).step_by(1_000) {
            let now = Clock {
                mono: t,
                wall: woke.wall + (t - 2_000),
            };
            assert_eq!(rec.tick(now), None, "no probe during grace");
        }
        assert_eq!(disk(&dir).sessions[0].stalls, 0);
        // a probe after the grace period that nobody answers is a real freeze
        let after = |t: u64| Clock {
            mono: t,
            wall: woke.wall + (t - 2_000),
        };
        assert_eq!(rec.tick(after(14_000)), Some(2));
        for t in 15_000..=19_000 {
            if t % 1_000 == 0 {
                rec.tick(after(t));
            }
        }
        assert_eq!(disk(&dir).sessions[0].stalls, 1);
        fs::remove_dir_all(&dir).unwrap();

        // the process itself is stopped for 40 seconds: both clocks move, but the beat was missed
        let dir = temp("paused");
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        rec.tick(at(1_000));
        for t in [41_000, 42_000, 43_000, 50_000] {
            assert_eq!(rec.tick(at(t)), None);
        }
        assert_eq!(disk(&dir).sessions[0].stalls, 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn turning_recording_off_and_on_does_not_turn_old_silence_into_a_stall() {
        let dir = temp("reset");
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        rec.tick(at(1_000));
        write_pref(&dir, false).unwrap();
        assert_eq!(rec.tick(at(2_000)), None);
        // off for a minute, with the probe from before never answered
        write_pref(&dir, true).unwrap();
        assert_eq!(
            rec.tick(at(62_000)),
            Some(2),
            "a fresh probe, not the old one"
        );
        rec.tick(at(63_000));
        assert_eq!(disk(&dir).sessions.iter().map(|s| s.stalls).sum::<u32>(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn off_records_nothing_and_stays_off_after_a_restart() {
        let dir = temp("off");
        write_pref(&dir, false).unwrap();
        let mut rec = recorder(&dir);
        assert_eq!(rec.tick(at(0)), None);
        assert!(
            !dir.join(JOURNAL).exists() && !dir.join(LEASE).exists(),
            "off touches no record"
        );
        assert_eq!(
            Status::from_u8(rec.counters.status.load(Relaxed)),
            Status::Off
        );

        // on, then a launch's worth of records, then off in the middle of it
        write_pref(&dir, true).unwrap();
        run_healthy(&mut rec, 1_000, 3_000);
        rec.counters.panics.store(1, Relaxed);
        write_pref(&dir, false).unwrap();
        rec.tick(at(4_000));
        let stopped = fs::read(dir.join(JOURNAL)).unwrap();
        assert_eq!(disk(&dir).sessions[0].end, End::Stopped);
        // more happens while it is off: a panic, a long silence, slow steps
        rec.counters.panics.store(5, Relaxed);
        rec.counters.slow[3].store(9, Relaxed);
        for t in (5_000..=60_000).step_by(5_000) {
            assert_eq!(rec.tick(at(t)), None);
        }
        assert_eq!(
            fs::read(dir.join(JOURNAL)).unwrap(),
            stopped,
            "nothing written while off"
        );

        // a restart reads the same choice
        let mut again = recorder(&dir);
        assert_eq!(again.tick(at(0)), None);
        assert_eq!(fs::read(dir.join(JOURNAL)).unwrap(), stopped);
        // its lock was let go, so another copy can take over
        write_pref(&dir, true).unwrap();
        assert!(Lease::acquire(&dir).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_setting_fails_closed_and_a_missing_one_means_on() {
        let dir = temp("unreadable");
        assert_eq!(
            read_pref(&dir),
            Pref::On,
            "no folder, no marker: the default"
        );
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_pref(&dir), Pref::On);
        write_pref(&dir, false).unwrap();
        assert_eq!(read_pref(&dir), Pref::Off);
        fs::remove_dir_all(&dir).unwrap();

        // a file where the folder should be: the marker can be neither found nor ruled out
        fs::write(&dir, "not a folder").unwrap();
        assert_eq!(read_pref(&dir), Pref::Unreadable);
        let mut rec = recorder(&dir);
        assert_eq!(rec.tick(at(0)), None);
        assert_eq!(
            Status::from_u8(rec.counters.status.load(Relaxed)),
            Status::PrefUnreadable
        );
        assert!(!rec.counters.enabled.load(Relaxed));
        fs::remove_file(&dir).unwrap();
    }

    #[test]
    fn a_choice_that_cannot_be_saved_still_stops_recording() {
        let dir = temp("unsaved");
        fs::write(&dir, "in the way").unwrap();
        let c = counters();
        apply_choice(Some(&dir), c, false);
        assert!(!c.enabled.load(Relaxed));
        assert!(c.forced_off.load(Relaxed));
        assert_eq!(
            make_view(&Journal::fresh(), c).problem,
            Some("stop_unsaved")
        );
        // the recorder honours it even though the marker never reached the disk
        let mut rec = Recorder::new(dir.join("inner"), c, "1.2.3");
        assert_eq!(rec.tick(at(0)), None);
        assert!(!rec.live);
        apply_choice(Some(&dir), c, true);
        assert_eq!(
            make_view(&Journal::fresh(), c).problem,
            Some("start_unsaved")
        );
        assert!(!c.enabled.load(Relaxed), "on counts only once it is saved");
        apply_choice(None, c, false);
        assert_eq!(
            make_view(&Journal::fresh(), c).problem,
            Some("stop_unsaved")
        );
        fs::remove_file(&dir).unwrap();

        // a choice that saves clears the problem and takes hold
        let ok = temp("saved");
        let c = counters();
        apply_choice(Some(&ok), c, false);
        assert!(
            ok.join(OFF_MARKER).exists() && !c.enabled.load(Relaxed) && !c.forced_off.load(Relaxed)
        );
        apply_choice(Some(&ok), c, true);
        assert!(!ok.join(OFF_MARKER).exists() && c.enabled.load(Relaxed));
        let view = make_view(&Journal::fresh(), c);
        assert!(view.keep && view.problem.is_none());
        fs::remove_dir_all(&ok).unwrap();
    }

    #[test]
    fn a_second_copy_never_writes_over_a_live_one_or_counts_it_unclean() {
        let dir = temp("lease");
        let mut first = recorder(&dir);
        first.tick(at(0));
        let before = fs::read(dir.join(JOURNAL)).unwrap();

        let mut second = recorder(&dir);
        assert_eq!(second.tick(at(0)), None);
        assert_eq!(
            Status::from_u8(second.counters.status.load(Relaxed)),
            Status::InUse
        );
        assert!(!second.live);
        assert_eq!(
            fs::read(dir.join(JOURNAL)).unwrap(),
            before,
            "the live copy's record is untouched"
        );
        assert_eq!(disk(&dir).sessions[0].end, End::Running);
        assert_eq!(totals(&disk(&dir)).unclean, 0);
        // the second copy can still read what is there
        assert_eq!(totals(&disk(&dir)).launches, 1);

        // the first dies without a word; the second takes over and finds it Running
        drop(first);
        second.tick(at(5_000));
        assert_eq!(
            Status::from_u8(second.counters.status.load(Relaxed)),
            Status::Recording
        );
        let journal = disk(&dir);
        assert_eq!(
            journal.sessions.iter().map(|s| s.end).collect::<Vec<_>>(),
            vec![End::Lost, End::Running]
        );
        assert_eq!(totals(&journal).unclean, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clean_exits_panics_and_unknown_exits_are_told_apart() {
        let dir = temp("classify");
        // launch 1: a normal quit
        let mut one = recorder(&dir);
        run_healthy(&mut one, 0, 2_000);
        one.finish_clean().unwrap();
        drop(one);
        // launch 2: a worker panics, then the process dies
        let mut two = recorder(&dir);
        run_healthy(&mut two, 0, 1_000);
        two.counters.panic_phase.store(BACKGROUND, Relaxed);
        two.counters.panics.store(1, Relaxed);
        two.tick(at(2_000));
        assert_eq!(
            totals(&disk(&dir)).panics,
            1,
            "written on the beat after the panic"
        );
        drop(two);
        // launch 3: finds launch 2 Running
        let mut three = recorder(&dir);
        three.tick(at(0));
        let journal = disk(&dir);
        assert_eq!(
            journal.sessions.iter().map(|s| s.end).collect::<Vec<_>>(),
            vec![End::Clean, End::Lost, End::Running]
        );
        let t = totals(&journal);
        assert_eq!((t.panics, t.stalls, t.unclean, t.launches), (1, 0, 1, 3));
        let panic = journal
            .events
            .iter()
            .find(|e| e.kind == Kind::Panic)
            .unwrap();
        assert_eq!(
            panic.phase,
            Phase::Background,
            "a worker panic is not pinned on a main thread step"
        );
        assert!(journal
            .events
            .iter()
            .any(|e| e.kind == Kind::Lost && e.s == journal.sessions[1].n));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_newest_launches_and_events_are_kept() {
        let dir = temp("retention");
        for _ in 0..30 {
            let mut rec = recorder(&dir);
            rec.tick(at(0));
            // each dies without a quit
        }
        let journal = disk(&dir);
        assert_eq!(journal.sessions.len(), MAX_SESSIONS);
        assert!(journal.events.len() <= MAX_EVENTS);
        assert!(
            journal.events.iter().all(|e| e.s >= journal.sessions[0].n),
            "no event outlives its launch"
        );
        assert!(fs::metadata(dir.join(JOURNAL)).unwrap().len() < MAX_JOURNAL_BYTES / 4);

        // a stall storm cannot grow the ring
        let mut rec = recorder(&dir);
        for _ in 0..200 {
            rec.journal.event(Kind::Stall, Phase::ConfigSave, 5);
        }
        assert_eq!(rec.journal.events.len(), MAX_EVENTS);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let counter = AtomicU32::new(u32::MAX - 1);
        bump(&counter);
        bump(&counter);
        assert_eq!(counter.load(Relaxed), u32::MAX);
        let mut journal = Journal::fresh();
        journal.sessions = vec![
            Session {
                panics: u32::MAX,
                stalls: u32::MAX,
                ..Session::default()
            };
            3
        ];
        journal.sanitize();
        let t = totals(&journal);
        assert_eq!(t.panics, 3 * u64::from(u32::MAX));
        assert!(render_report(&journal, false, "1.0").contains(&format!("Panics: {}", t.panics)));
    }

    #[test]
    fn a_hostile_or_oversized_file_is_neither_trusted_nor_exported() {
        let dir = temp("hostile");
        fs::create_dir_all(&dir).unwrap();
        let hostile = r#"{"v":1,"next":4294967295,"extra":"/Users/alice/secret.txt",
            "sessions":[{"n":7,"build":"/Users/alice/Documents/reminder: call mum","end":"Exploded",
            "panics":4294967295,"stalls":3,"slow":[1,2,3],"note":"alice@example.com"},
            {"n":8,"build":"1.0.7\u001b[31m","end":"Lost","slow":[]}],
            "events":[{"s":7,"kind":"Shell","phase":"RmRf","secs":99999,"text":"/etc/passwd"},
            {"s":99,"kind":"Stall","phase":"TrayMenu","secs":61}]}"#;
        fs::write(dir.join(JOURNAL), hostile).unwrap();
        let (journal, load) = load_journal(&dir);
        assert_eq!(load, Load::Ok);
        assert!(journal.sessions.iter().all(|s| s.slow.len() == SLOTS));
        let text = render_report(&journal, true, "1.0.7");
        for forbidden in [
            "alice", "Users", "/", "\\", "@", "secret", "passwd", "mum", "\u{1b}", "Exploded",
            "RmRf",
        ] {
            assert!(
                !text.contains(forbidden),
                "{forbidden:?} reached the report:\n{text}"
            );
        }
        assert!(
            text.bytes()
                .all(|b| b == b'\n' || (0x20..0x7f).contains(&b)),
            "plain ASCII only"
        );
        assert!(text.contains("Versions: unknown"));
        assert!(text.contains("Unclean exits (no normal quit was recorded, cause unknown): 1"));

        // too big to be ours: set aside, and recording starts from nothing
        let big = format!("{}{}", " ".repeat(MAX_JOURNAL_BYTES as usize), r#"{"v":1}"#);
        fs::write(dir.join(JOURNAL), &big).unwrap();
        assert_eq!(load_journal(&dir).1, Load::Bad);
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        assert_eq!(fs::read(dir.join(JOURNAL_BAD)).unwrap().len(), big.len());
        assert_eq!(disk(&dir).sessions.len(), 1);

        // not JSON, and a format from some other version, are set aside too
        for text in [
            "{ not json",
            r#"{"v":2,"sessions":[]}"#,
            "",
            r#"{"sessions":[]}"#,
        ] {
            fs::write(dir.join(JOURNAL), text).unwrap();
            assert_eq!(load_journal(&dir).1, Load::Bad, "{text:?}");
        }
        // deep nesting is refused by the parser, not recursed into
        fs::write(
            dir.join(JOURNAL),
            format!("{}{}", "[".repeat(5000), "]".repeat(5000)),
        )
        .unwrap();
        assert_eq!(load_journal(&dir).1, Load::Bad);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unwritable_folder_gives_a_fixed_status_and_does_not_panic() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let parent = temp("unwritable");
            fs::create_dir_all(&parent).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
            // a user who can write anyway (root) cannot show this
            if fs::create_dir(parent.join("probe")).is_err() {
                let mut rec = recorder(&parent.join("inner"));
                assert_eq!(rec.tick(at(0)), None);
                assert_eq!(
                    Status::from_u8(rec.counters.status.load(Relaxed)),
                    Status::Unwritable
                );
                assert!(!rec.live);
            }
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            fs::remove_dir_all(&parent).unwrap();
            assert_eq!(
                Status::of_error(&io::Error::from_raw_os_error(28)),
                Status::Full
            );
            assert_eq!(
                Status::of_error(&io::Error::from_raw_os_error(13)),
                Status::Unwritable
            );
        }
        assert_eq!(Status::Unwritable.code(), "unwritable");
        assert_eq!(Status::Full.code(), "disk_full");
    }

    #[test]
    fn a_write_that_fails_part_way_leaves_the_old_record_and_is_tried_again() {
        let dir = temp("retry");
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        let good = fs::read(dir.join(JOURNAL)).unwrap();
        // a folder where the temporary file goes makes the next write fail
        fs::create_dir(dir.join(JOURNAL_TMP)).unwrap();
        for t in [1_000, 2_000, 3_000, 4_000, 5_000] {
            rec.tick(at(t));
        }
        assert_eq!(
            fs::read(dir.join(JOURNAL)).unwrap(),
            good,
            "the old record is whole"
        );
        assert!(rec.dirty && rec.live, "kept in memory");
        assert_eq!(
            Status::from_u8(rec.counters.status.load(Relaxed)),
            Status::Unwritable
        );
        fs::remove_dir(dir.join(JOURNAL_TMP)).unwrap();
        rec.tick(at(6_000));
        assert!(!rec.dirty);
        assert_eq!(
            disk(&dir).sessions[0].stalls,
            1,
            "the stall that happened meanwhile is not lost"
        );
        assert_eq!(
            Status::from_u8(rec.counters.status.load(Relaxed)),
            Status::Recording
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn slow_steps_are_counted_by_length_and_only_while_recording() {
        let _guard = GLOBAL.lock().unwrap();
        MAIN.with(|m| m.set(true));
        COUNTERS.enabled.store(true, Relaxed);
        let index = slot(Phase::ConfigSave, 0).unwrap();
        let before = COUNTERS.slow[index].load(Relaxed);
        {
            let outer = span(Phase::TrayRefresh);
            assert_eq!(COUNTERS.phase.load(Relaxed), Phase::TrayRefresh as u8);
            {
                let _inner = span(Phase::ConfigSave);
                assert_eq!(COUNTERS.phase.load(Relaxed), Phase::ConfigSave as u8);
                std::thread::sleep(Duration::from_millis(130));
            }
            assert_eq!(
                COUNTERS.phase.load(Relaxed),
                Phase::TrayRefresh as u8,
                "the outer step is named again"
            );
            drop(outer);
        }
        assert_eq!(COUNTERS.phase.load(Relaxed), 0);
        assert_eq!(COUNTERS.slow[index].load(Relaxed), before + 1);
        // a fast step counts for nothing
        {
            let _quick = span(Phase::ConfigSave);
        }
        assert_eq!(COUNTERS.slow[index].load(Relaxed), before + 1);
        COUNTERS.enabled.store(false, Relaxed);
        {
            let _off = span(Phase::ConfigSave);
            std::thread::sleep(Duration::from_millis(130));
        }
        assert_eq!(
            COUNTERS.slow[index].load(Relaxed),
            before + 1,
            "nothing counted while off"
        );
        COUNTERS.enabled.store(true, Relaxed);
        // off the main thread a span times and names nothing
        MAIN.with(|m| m.set(false));
        {
            let _x = span(Phase::Quit);
            assert_eq!(COUNTERS.phase.load(Relaxed), 0);
        }
        assert_eq!(bucket(Duration::from_millis(99)), None);
        assert_eq!(bucket(Duration::from_millis(100)), Some(0));
        assert_eq!(bucket(Duration::from_millis(999)), Some(1));
        assert_eq!(bucket(Duration::from_secs(5)), Some(3));
        assert_eq!(bucket(Duration::from_secs(5000)), Some(3));
    }

    #[test]
    fn timing_counters_reach_the_record_on_a_slow_cadence() {
        let dir = temp("timing");
        let mut rec = recorder(&dir);
        run_healthy(&mut rec, 0, 1_000);
        let a = slot(Phase::SettingsOpen, 2).unwrap();
        rec.counters.slow[a].store(2, Relaxed);
        run_healthy(&mut rec, 2_000, 20_000);
        assert_eq!(
            disk(&dir).sessions[0].slow[a],
            0,
            "not rewritten every beat"
        );
        run_healthy(&mut rec, 21_000, 40_000);
        assert_eq!(disk(&dir).sessions[0].slow[a], 2);
        let text = render_report(&disk(&dir), true, "1.2.3");
        assert!(text.contains("- opening Settings: 1 to 5 s x2"), "{text}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_panic_leaves_a_count_and_never_its_message() {
        let _guard = GLOBAL.lock().unwrap();
        let dir = temp("panic");
        let secret = "SENTINEL-alice /Users/alice/reminder-call-mum.txt";
        install_panic_hook();
        COUNTERS.enabled.store(true, Relaxed);
        let before = COUNTERS.panics.load(Relaxed);
        let result = std::thread::Builder::new()
            .name("worker-of-alice".into())
            .spawn(move || panic!("{secret}"))
            .unwrap()
            .join();
        assert!(result.is_err());
        assert_eq!(COUNTERS.panics.load(Relaxed), before + 1);
        assert_eq!(COUNTERS.panic_phase.load(Relaxed), BACKGROUND);

        let mut rec = Recorder::new(dir.clone(), &COUNTERS, "1.2.3");
        // the counter already stood above zero when this launch began; one more panic follows it
        rec.tick(at(0));
        let _ = std::thread::spawn(|| panic!("second {}", "SENTINEL-alice")).join();
        rec.tick(at(1_000));
        let raw = String::from_utf8(fs::read(dir.join(JOURNAL)).unwrap()).unwrap();
        let text = report_of(&dir);
        for haystack in [&raw, &text] {
            for needle in ["SENTINEL", "alice", "Users", "mum", "worker-of"] {
                assert!(!haystack.contains(needle), "{needle:?} leaked:\n{haystack}");
            }
        }
        assert_eq!(disk(&dir).sessions[0].panics, 1);
        assert!(text.contains("Panics: 1"));
        COUNTERS.enabled.store(false, Relaxed);
        let _ = std::thread::spawn(|| panic!("while off")).join();
        assert_eq!(
            COUNTERS.panics.load(Relaxed),
            before + 2,
            "a panic while off is not counted"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    fn report_of(dir: &Path) -> String {
        render_report(&load_journal(dir).0, true, "1.2.3")
    }

    #[test]
    fn the_reviewed_report_is_the_exact_text_that_gets_copied() {
        let dir = temp("exact");
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        rec.counters.phase.store(Phase::OverlayOpen as u8, Relaxed);
        for t in 1..=6 {
            rec.tick(at(t * 1_000));
        }
        let first = report_of(&dir);
        assert_eq!(
            first,
            report_of(&dir),
            "reviewing twice shows the same bytes"
        );
        // what crosses to the page and back is the same string, line breaks included
        let sent: String = serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(sent.as_bytes(), first.as_bytes());
        assert!(first.contains("UI stalls (the app did not respond for 5 seconds or more): 1"));
        assert!(first.contains("UI stall (step: opening a reminder)"));
        assert!(!first.contains('\r'));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quitting_waits_a_short_time_for_the_record_and_says_when_it_could_not() {
        let started = Instant::now();
        assert_eq!(
            request_clean(counters(), None, Duration::from_secs(5)),
            CleanExit::Recorded,
            "no recorder, nothing to wait for"
        );
        assert!(started.elapsed() < Duration::from_millis(100));

        // a worker that never answers
        let silent = std::thread::spawn(|| std::thread::park_timeout(Duration::from_secs(2)));
        let started = Instant::now();
        assert_eq!(
            request_clean(counters(), Some(silent.thread()), Duration::from_millis(60)),
            CleanExit::TimedOut
        );
        assert!(
            started.elapsed() >= Duration::from_millis(60)
                && started.elapsed() < Duration::from_millis(400)
        );
        silent.thread().unpark();
        silent.join().unwrap();

        // a worker that answers, and one whose write failed
        for (ack, expected) in [(1, CleanExit::Recorded), (2, CleanExit::Failed)] {
            let c = counters();
            let answering = std::thread::spawn(move || {
                while !c.clean_requested.load(Acquire) {
                    std::thread::park_timeout(Duration::from_millis(5));
                }
                c.clean_ack.store(ack, Release);
            });
            assert_eq!(
                request_clean(c, Some(answering.thread()), CLEAN_EXIT_WAIT),
                expected
            );
            answering.join().unwrap();
        }
    }

    #[test]
    fn a_failed_final_write_is_reported_and_left_to_read_as_unclean() {
        let dir = temp("final");
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        fs::create_dir(dir.join(JOURNAL_TMP)).unwrap();
        assert!(
            rec.finish_clean().is_err(),
            "the failure is returned, not hidden"
        );
        assert_eq!(disk(&dir).sessions[0].end, End::Running);
        fs::remove_dir(dir.join(JOURNAL_TMP)).unwrap();
        drop(rec);
        let mut next = recorder(&dir);
        next.tick(at(0));
        assert_eq!(totals(&disk(&dir)).unclean, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stale_view_without_a_folder_is_empty_not_an_error() {
        let t = totals(&load_journal(&temp("nothing")).0);
        assert_eq!((t.panics, t.stalls, t.unclean, t.launches), (0, 0, 0, 0));
        let text = render_report(&Journal::fresh(), false, "1.0.7");
        assert!(text.contains("Recording now: no") && text.contains("Panics: 0"));
    }
}
