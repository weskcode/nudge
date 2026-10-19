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
// - a record that cannot be read, or is not one this version understands, is
//   never replaced, renamed or copied: recording stays off and says why.

use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::*};
use std::sync::{Mutex, Once, OnceLock, PoisonError, TryLockError};
use std::thread::Thread;
use std::time::{Duration, Instant, SystemTime};

const SCHEMA: u32 = 1;
const MAX_SESSIONS: usize = 20;
const MAX_EVENTS: usize = 96;
// the journal is a few kilobytes; anything near this size is not ours
const MAX_JOURNAL_BYTES: u64 = 128 * 1024;
const MAX_BUILDS_LISTED: usize = 8;

const JOURNAL: &str = "journal.json";
const JOURNAL_TMP: &str = "journal.json.tmp";
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
// the gap between tries at the final write of a launch that was switched off
const CLOSE_RETRY_MS: u64 = 10_000;
// how long switching off waits for a record write already under way
const COMMIT_WAIT: Duration = Duration::from_millis(250);
// how long quitting waits for the final record
const CLEAN_EXIT_WAIT: Duration = Duration::from_millis(300);

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

// every step before Background can be timed
const PHASE_COUNT: usize = Phase::Background as usize;
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

// everything a hook on the main thread or in the panic hook reads or writes is
// an atomic here: no lock, no allocation, no disk
struct Counters {
    phase: AtomicU8,
    // set once the app has started to quit, and kept through the event loop's cleanup
    quitting: AtomicBool,
    panics: AtomicU32,
    panic_phase: AtomicU8,
    slow: [AtomicU32; SLOTS],
    // the last probe the main thread answered
    answered_probe: AtomicU64,
    // bit 0: whether anything may be recorded. the rest counts changes, so a
    // decision made from an older reading can be told apart and dropped
    policy: AtomicU32,
    // an off choice that could not be saved: stay off until quit
    off_until_quit: AtomicBool,
    status: AtomicU8,
    problem: AtomicU8,
    // held while a record write is committed, so switching off can wait for one
    commit: Mutex<()>,
    clean_requested: AtomicBool,
    clean_ack: AtomicU8,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU32 = AtomicU32::new(0);

impl Counters {
    const fn new() -> Self {
        Counters {
            phase: AtomicU8::new(0),
            quitting: AtomicBool::new(false),
            panics: AtomicU32::new(0),
            panic_phase: AtomicU8::new(BACKGROUND),
            slow: [ZERO; SLOTS],
            answered_probe: AtomicU64::new(0),
            policy: AtomicU32::new(1),
            off_until_quit: AtomicBool::new(false),
            status: AtomicU8::new(Status::Recording as u8),
            problem: AtomicU8::new(0),
            commit: Mutex::new(()),
            clean_requested: AtomicBool::new(false),
            clean_ack: AtomicU8::new(0),
        }
    }

    fn enabled(&self) -> bool {
        self.policy.load(Acquire) & 1 == 1
    }

    // a person's choice: takes hold at once and wins over any reading in flight
    fn choose(&self, on: bool) {
        let _ = self.policy.fetch_update(AcqRel, Acquire, |p| {
            Some((p & !1).wrapping_add(2) | u32::from(on))
        });
    }

    // a decision the recorder made from a reading taken when the policy was
    // `seen`. it only counts if nothing was chosen since; the new value is returned
    fn adopt(&self, seen: u32, on: bool) -> Option<u32> {
        let next = if on { seen | 1 } else { seen & !1 };
        self.policy
            .compare_exchange(seen, next, AcqRel, Acquire)
            .ok()
            .map(|_| next)
    }

    fn current_phase(&self) -> Phase {
        if self.quitting.load(Relaxed) {
            Phase::Quit
        } else {
            Phase::from_u8(self.phase.load(Relaxed))
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
// one choice at a time, so a quick on and off reach the files in order
static CHOICE: Mutex<()> = Mutex::new(());

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
        if COUNTERS.enabled() {
            if let Some(index) = bucket(self.start.elapsed()).and_then(|b| slot(self.phase, b)) {
                bump(&COUNTERS.slow[index]);
            }
        }
    }
}

// the app has started to quit: a hang from here to the end of cleanup is named as quitting
pub fn quit_started() {
    COUNTERS.quitting.store(true, Relaxed);
}

// whether a quit has begun. a Windows shutdown never gets here, so the Exit
// callback can tell it apart from a quit that ExitRequested already recorded
#[cfg(target_os = "windows")]
pub fn quitting() -> bool {
    COUNTERS.quitting.load(Relaxed)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Status {
    Recording,
    Off,
    PrefUnreadable,
    InUse,
    Full,
    Unwritable,
    // the saved record is there but is not one this version can use; left as it is
    HistoryInvalid,
    // the saved record could not be read; left as it is
    HistoryUnreadable,
    // off, but the last write that closes this launch has not succeeded yet
    ClosePending,
}

impl Status {
    fn from_u8(value: u8) -> Status {
        match value {
            0 => Status::Recording,
            1 => Status::Off,
            2 => Status::PrefUnreadable,
            3 => Status::InUse,
            4 => Status::Full,
            6 => Status::HistoryInvalid,
            7 => Status::HistoryUnreadable,
            8 => Status::ClosePending,
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
            Status::HistoryInvalid => "history_invalid",
            Status::HistoryUnreadable => "history_unreadable",
            Status::ClosePending => "close_pending",
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

    // what the page is told. recording is only claimed while the policy allows it
    fn shown(counters: &Counters) -> Status {
        let stored = Status::from_u8(counters.status.load(Relaxed));
        let still_off = matches!(
            stored,
            Status::Off | Status::PrefUnreadable | Status::ClosePending
        );
        if counters.enabled() || still_off {
            stored
        } else {
            Status::Off
        }
    }
}

const PROBLEM_NONE: u8 = 0;
const PROBLEM_STOP_UNSAVED: u8 = 1;
const PROBLEM_START_UNSAVED: u8 = 2;

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
    #[serde(alias = "s")]
    launch: u32,
    kind: Kind,
    phase: Phase,
    // whole seconds in steps of 5
    secs: u32,
}

impl Default for Event {
    fn default() -> Self {
        Event {
            launch: 0,
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

// a version from a file or the build: kept only as three plain numbers, so
// that nothing a person could have typed or a system filled in passes as one
fn clean_build(text: &str) -> String {
    let mut parts = text.split('.');
    let plain = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|p| (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
    }) && parts.next().is_none();
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
            self.events.retain(|e| e.launch >= oldest);
        }
        if self.events.len() > MAX_EVENTS {
            self.events.drain(..self.events.len() - MAX_EVENTS);
        }
    }

    fn event(&mut self, kind: Kind, phase: Phase, secs: u32) {
        let launch = self.sessions.last().map_or(0, |s| s.n);
        self.events.push(Event {
            launch,
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
    Valid(Journal),
    // too big, not JSON, or another format: not ours to read or to replace
    Invalid,
    // the file is there but could not be opened or read
    Unreadable,
}

impl Load {
    // what to count and show when there is nothing usable
    fn journal(self) -> Journal {
        match self {
            Load::Valid(journal) => journal,
            _ => Journal::fresh(),
        }
    }
}

fn load_journal(dir: &Path) -> Load {
    let file = match File::open(dir.join(JOURNAL)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Load::Missing,
        Err(_) => return Load::Unreadable,
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Load::Unreadable;
    }
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Load::Invalid;
    }
    match serde_json::from_slice::<Journal>(&bytes) {
        Ok(mut journal) if journal.v == SCHEMA => {
            journal.sanitize();
            Load::Valid(journal)
        }
        _ => Load::Invalid,
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

// the choice lives in a file of its own, apart from config.json: it has to be
// readable when settings are not, and a second copy of Nudge can change it
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pref {
    On,
    Off,
    // present but unreadable is not the same as absent: that stays off
    Unreadable,
}

// the choice is kept inside the folder and also beside it. deleting the folder
// to start over removes only the first copy, so it cannot turn recording back on.
// the second copy is named after the folder, so other folders never share it
fn pref_markers(dir: &Path) -> [PathBuf; 2] {
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    [
        dir.join(OFF_MARKER),
        dir.with_file_name(format!("{name}.off")),
    ]
}

// off if either copy says so; unreadable (not merely absent) fails closed
fn read_pref(dir: &Path) -> Pref {
    let mut pref = Pref::On;
    for marker in pref_markers(dir) {
        match fs::symlink_metadata(marker) {
            Ok(_) => return Pref::Off,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => pref = Pref::Unreadable,
        }
    }
    pref
}

fn write_pref(dir: &Path, on: bool) -> io::Result<()> {
    let [inside, beside] = pref_markers(dir);
    if on {
        for marker in [inside, beside] {
            match fs::remove_file(&marker) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        Ok(())
    } else {
        // a choice is saved in both places or not at all, so a failure part way
        // leaves nothing behind that could be read as a saved Off
        // a failed write can leave an empty file behind, which reads as Off, so
        // whatever was started is removed again unless Off was already saved
        let was_off = inside.is_file() || beside.is_file();
        let saved = fs::create_dir_all(dir)
            .and_then(|()| fs::write(&inside, b"off\n"))
            .and_then(|()| match beside.parent() {
                Some(parent) => fs::create_dir_all(parent),
                None => Ok(()),
            })
            .and_then(|()| fs::write(&beside, b"off\n"));
        if let Err(error) = saved {
            if !was_off {
                let _ = fs::remove_file(&inside);
                let _ = fs::remove_file(&beside);
            }
            return Err(error);
        }
        Ok(())
    }
}

struct Lease {
    _file: File,
}

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
        Ok(Lease { _file: file })
    }
}

// false once the lock file has been deleted, or replaced by another copy's. the
// folder can be deleted while Nudge runs, and then this launch's lock guards nothing
#[cfg(unix)]
fn lease_current(dir: &Path, lease: &Lease) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(dir.join(LEASE)), lease._file.metadata()) {
        (Ok(path), Ok(held)) => path.dev() == held.dev() && path.ino() == held.ino(),
        _ => false,
    }
}

// on Windows the open lock file cannot be deleted, so it cannot go stale
#[cfg(not(unix))]
fn lease_current(_: &Path, _: &Lease) -> bool {
    true
}

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
    // the probe in flight and when its wait began. a probe already queued on
    // the main thread cannot be recalled, so it is never replaced before it is answered
    pending: Option<(u64, u64)>,
    grace_until: u64,
    stalled: Option<Phase>,
    restart: bool,
}

impl Watch {
    // forget the silence so far, so turning recording off and on again cannot
    // read an old silence as a freeze. the probe in flight stays the one to wait for
    fn reset(&mut self) {
        self.last = None;
        self.stalled = None;
        self.restart = true;
    }

    fn rewait(&mut self, since: u64) {
        if let Some((seq, _)) = self.pending {
            self.pending = Some((seq, since));
        }
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
                // a stall already recorded stays recorded, and its wait keeps counting
                // from when the freeze began, so the same unanswered probe cannot
                // count as a second stall and the recovery reports the whole wait
                if self.stalled.is_none() {
                    self.rewait(self.grace_until);
                }
            }
        }
        self.last = Some(now);
        if std::mem::take(&mut self.restart) {
            self.rewait(now.mono.max(self.grace_until));
        }
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

struct Recorder {
    dir: PathBuf,
    counters: &'static Counters,
    build: &'static str,
    lease: Option<Lease>,
    journal: Journal,
    // this launch has a Running record open
    live: bool,
    // the launch whose lock was lost while it was live. when the lock is held
    // again, its record is continued in memory, not restarted, so its counts
    // and timings are kept and it is not marked as lost
    own: Option<u32>,
    // recording went off: the record is closed as Stopped in memory, and
    // nothing more is collected until that write succeeds
    closing: bool,
    close_retry_at: u64,
    dirty: bool,
    base_panics: u32,
    seen_panics: u32,
    base_slow: [u32; SLOTS],
    saved_slow: [u32; SLOTS],
    saved_at: u64,
    watch: Watch,
    // tests hold the recorder here, after its reads and before it commits a write
    #[cfg(test)]
    gate: Option<Box<dyn Fn() + Send>>,
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
            own: None,
            closing: false,
            close_retry_at: 0,
            dirty: false,
            base_panics: 0,
            seen_panics: 0,
            base_slow: [0; SLOTS],
            saved_slow: [0; SLOTS],
            saved_at: 0,
            watch: Watch::default(),
            #[cfg(test)]
            gate: None,
        }
    }

    fn set_status(&self, status: Status) {
        self.counters.status.store(status as u8, Relaxed);
    }

    fn pref(&self) -> Pref {
        if self.counters.off_until_quit.load(Acquire) {
            Pref::Off
        } else {
            read_pref(&self.dir)
        }
    }

    // one beat: follow the preference, record what happened, and say which
    // probe to send the main thread
    fn tick(&mut self, now: Clock) -> Option<u64> {
        let seen = self.counters.policy.load(Acquire);
        let pref = self.pref();
        // a choice made while the setting was being read wins; read it again next beat
        let policy = self.counters.adopt(seen, pref == Pref::On)?;
        // the lock file was deleted or replaced, so nothing more is written under it.
        // a live launch is continued below if its record is still on disk, and
        // otherwise a new launch begins. this also covers a lock held without a live
        // launch, such as after an unusable record
        if self.lease.as_ref().is_some_and(|lease| !lease_current(&self.dir, lease)) {
            // the launch is remembered so that begin can continue its record
            if self.live {
                self.own = self.journal.sessions.last().map(|s| s.n);
            }
            self.live = false;
            self.closing = false;
            self.lease = None;
            self.watch.reset();
        }
        if pref != Pref::On {
            if self.stop(now.mono) {
                self.set_status(if pref == Pref::Unreadable {
                    Status::PrefUnreadable
                } else {
                    Status::Off
                });
            }
            return None;
        }
        if self.closing && !self.stop(now.mono) {
            return None;
        }
        if !self.live && !self.begin(policy) {
            return None;
        }

        let panics = self.counters.panics.load(Relaxed);
        if panics > self.seen_panics {
            self.seen_panics = panics;
            let phase = Phase::from_u8(self.counters.panic_phase.load(Relaxed));
            self.journal.event(Kind::Panic, phase, 0);
            self.dirty = true;
        }

        let (post, seen) = self.watch.step(
            now,
            self.counters.answered_probe.load(Acquire),
            self.counters.current_phase(),
        );
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
            self.persist(now.mono, policy);
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

    // a write that starts or extends this launch's record. it is skipped when a
    // choice was made since `policy` was read, so an acknowledged off cannot be
    // followed by a record that off did not see
    fn commit(&self, journal: &Journal, policy: u32) -> Option<io::Result<()>> {
        #[cfg(test)]
        if let Some(gate) = &self.gate {
            gate();
        }
        let _held = self
            .counters
            .commit
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // the lock file must still be the one we hold: if the folder was deleted and
        // another copy has since recorded, writing now would overwrite its record
        let held = self
            .lease
            .as_ref()
            .is_some_and(|lease| lease_current(&self.dir, lease));
        (held && self.counters.policy.load(Acquire) == policy).then(|| write_journal(&self.dir, journal))
    }

    // a launch starts: take the lock, settle what earlier launches left
    // behind, and add this one. false while it cannot be done
    fn begin(&mut self, policy: u32) -> bool {
        if self.lease.is_none() {
            match Lease::acquire(&self.dir) {
                Ok(lease) => self.lease = Some(lease),
                Err(error) => {
                    // another copy holds the lock now, so this launch is no longer ours
                    if matches!(error, LeaseError::Busy) {
                        self.own = None;
                    }
                    self.fail(error);
                    return false;
                }
            }
        }
        // a record that cannot be read or understood is left exactly as it is:
        // starting over would replace history that may only be unreachable for now
        let mut journal = match load_journal(&self.dir) {
            Load::Missing => Journal::fresh(),
            Load::Valid(journal) => journal,
            Load::Invalid => {
                self.set_status(Status::HistoryInvalid);
                return false;
            }
            Load::Unreadable => {
                self.set_status(Status::HistoryUnreadable);
                return false;
            }
        };
        // a launch whose lock was lost while it was recording continues its record
        // as it was in memory, which holds counts and timings not yet saved
        // continued only while its record is still on disk. a deleted folder starts
        // a new launch instead, so nothing from memory is merged into a fresh record
        let own = self
            .own
            .filter(|n| journal.sessions.iter().any(|s| s.n == *n));
        let mine = own.and_then(|n| self.journal.sessions.iter().find(|s| s.n == n).cloned());
        // a launch that was closing (recording off, close not yet written) is written as Stopped
        let closed = mine.as_ref().is_some_and(|s| s.end == End::Stopped);
        // holding the lock means no other launch is alive, so one still marked
        // Running ended without a recorded quit, except the launch being continued
        let lost: Vec<u32> = journal
            .sessions
            .iter_mut()
            .filter(|s| s.end == End::Running && Some(s.n) != own)
            .map(|s| {
                s.end = End::Lost;
                s.n
            })
            .collect();
        for n in lost {
            journal.events.push(Event {
                launch: n,
                kind: Kind::Lost,
                phase: Phase::Unknown,
                secs: 0,
            });
        }
        let resumed = match (own, mine) {
            (Some(n), Some(session)) => {
                // memory holds every event of this launch, so it replaces the saved set
                let events: Vec<Event> = self
                    .journal
                    .events
                    .iter()
                    .filter(|e| e.launch == n)
                    .cloned()
                    .collect();
                journal.events.retain(|e| e.launch != n);
                journal.events.extend(events);
                journal.sessions.retain(|s| s.n != n);
                journal.sessions.push(session);
                true
            }
            _ => {
                journal.sessions.push(Session {
                    n: journal.next,
                    build: clean_build(self.build),
                    end: End::Running,
                    slow: vec![0; SLOTS],
                    ..Session::default()
                });
                journal.next = journal.next.saturating_add(1);
                false
            }
        };
        journal.sanitize();
        match self.commit(&journal, policy) {
            // a choice came in while the file was being read: decide again next beat
            None => return false,
            Some(Err(error)) => {
                self.set_status(Status::of_error(&error));
                return false;
            }
            Some(Ok(())) => {}
        }
        if resumed && closed {
            // the Stopped record is saved; this copy lets the lock go and begins
            // nothing further, so the launch is not reported as lost
            self.journal = journal;
            self.own = None;
            self.lease = None;
            self.live = false;
            self.closing = false;
            return false;
        }
        self.journal = journal;
        self.live = true;
        self.own = None;
        self.dirty = false;
        // a continued launch keeps the baselines its counts are measured from
        if !resumed {
            self.base_panics = self.counters.panics.load(Relaxed);
            self.seen_panics = self.base_panics;
            self.base_slow = self.counters.slow_now();
            self.saved_slow = [0; SLOTS];
        }
        self.watch.reset();
        self.set_status(Status::Recording);
        true
    }

    fn persist(&mut self, mono: u64, policy: u32) {
        self.sync_session();
        match self.commit(&self.journal, policy) {
            None => {}
            Some(Ok(())) => {
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
            Some(Err(error)) => self.set_status(Status::of_error(&error)),
        }
    }

    // freeze this launch's counts as Stopped; the write comes after
    fn mark_stopped(&mut self, mono: u64) {
        if self.closing {
            return;
        }
        self.sync_session();
        if let Some(session) = self.journal.sessions.last_mut() {
            session.end = End::Stopped;
        }
        self.closing = true;
        self.close_retry_at = mono;
    }

    fn close(&mut self) -> io::Result<()> {
        if !self.lease.as_ref().is_some_and(|lease| lease_current(&self.dir, lease)) {
            return Err(io::Error::other("the lock file is no longer this copy's"));
        }
        write_journal(&self.dir, &self.journal)?;
        self.live = false;
        self.closing = false;
        Ok(())
    }

    // recording is off: close this launch as Stopped with one last write, which
    // saves the timings gathered since the last save, and let the lock go so
    // another copy can take over. while that write fails the launch stays ours
    // (lock kept, nothing new collected) and the write is tried again every
    // CLOSE_RETRY_MS. true once nothing is left to close
    fn stop(&mut self, mono: u64) -> bool {
        self.watch.reset();
        self.own = None;
        if self.live {
            self.mark_stopped(mono);
            if mono < self.close_retry_at {
                return false;
            }
            if self.close().is_err() {
                self.close_retry_at = mono.saturating_add(CLOSE_RETRY_MS);
                self.set_status(Status::ClosePending);
                return false;
            }
        }
        self.lease = None;
        true
    }

    // the app is returning from its event loop: close this launch as clean
    fn finish_clean(&mut self) -> io::Result<()> {
        if !self.live {
            return Ok(());
        }
        // the record is no longer this copy's to write, so it is left as the other copy has it
        if !self.lease.as_ref().is_some_and(|lease| lease_current(&self.dir, lease)) {
            return Ok(());
        }
        if self.closing || self.pref() != Pref::On {
            self.mark_stopped(0);
            return self.close();
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
// labels, so nothing in the file can reach it as text. `saved` says why the
// counts are empty when the saved record could not be used
fn render_report(journal: &Journal, recording: bool, build: &str, saved: Option<&str>) -> String {
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
    if let Some(saved) = saved {
        let _ = writeln!(out, "Saved record: {saved}");
    }
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
        let ago = match latest.saturating_sub(event.launch) {
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
    Status::shown(&COUNTERS) == Status::Recording
}

fn make_view(journal: &Journal, counters: &Counters) -> View {
    let t = totals(journal);
    View {
        keep: counters.enabled(),
        status: Status::shown(counters).code(),
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

fn load_here() -> Load {
    dir().map_or(Load::Missing, load_journal)
}

fn view() -> View {
    make_view(&load_here().journal(), &COUNTERS)
}

fn report() -> String {
    let load = load_here();
    let saved = match load {
        Load::Invalid => {
            Some("not in a form this version of Nudge can read, so the counts below are empty")
        }
        Load::Unreadable => Some("could not be read, so the counts below are empty"),
        _ => None,
    };
    render_report(
        &load.journal(),
        recording_now(),
        env!("CARGO_PKG_VERSION"),
        saved,
    )
}

// switching off gates the hooks before anything else, then waits a moment for
// a record write already under way, then saves the choice. a choice that
// cannot be saved still stops recording, until quit. switching on only counts
// once it is saved
fn apply_choice(dir: Option<&Path>, counters: &Counters, on: bool) {
    let _alone = CHOICE.lock().unwrap_or_else(PoisonError::into_inner);
    let saved = |on| match dir {
        Some(dir) => write_pref(dir, on),
        None => Err(io::Error::from(io::ErrorKind::NotFound)),
    };
    if on {
        match saved(true) {
            Ok(()) => {
                counters.off_until_quit.store(false, Release);
                counters.problem.store(PROBLEM_NONE, Relaxed);
                counters.choose(true);
            }
            Err(_) => counters.problem.store(PROBLEM_START_UNSAVED, Relaxed),
        }
        return;
    }
    counters.off_until_quit.store(true, Release);
    counters.choose(false);
    let until = Instant::now() + COMMIT_WAIT;
    loop {
        match counters.commit.try_lock() {
            Err(TryLockError::WouldBlock) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(5))
            }
            _ => break,
        }
    }
    match saved(false) {
        Ok(()) => {
            counters.off_until_quit.store(false, Release);
            counters.problem.store(PROBLEM_NONE, Relaxed);
        }
        Err(_) => counters.problem.store(PROBLEM_STOP_UNSAVED, Relaxed),
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
        COUNTERS.choose(pref == Pref::On);
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
    if !COUNTERS.enabled() {
        return;
    }
    let phase = if on_main() {
        COUNTERS.current_phase() as u8
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
            let _ = app.run_on_main_thread(move || {
                COUNTERS.answered_probe.fetch_max(seq, Release);
            });
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
// path. on macOS, watch_terminate calls it too, and on Windows the Exit callback
// does when no quit began. waits at most 300 ms; a record that could not be
// written leaves this launch to be counted as unclean, which is what it will look like
pub fn mark_clean_exit() {
    match request_clean(&COUNTERS, WORKER.get(), CLEAN_EXIT_WAIT) {
        CleanExit::Recorded => {}
        // stderr, not println!: a closed pipe makes println! panic, and this runs
        // inside the macOS notification block. the write error is ignored on purpose
        CleanExit::Failed => {
            let _ = writeln!(
                io::stderr(),
                "diagnostics: the normal quit could not be recorded"
            );
        }
        CleanExit::TimedOut => {
            let _ = writeln!(
                io::stderr(),
                "diagnostics: the normal quit was not recorded in time"
            );
        }
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

    fn off_on_disk(dir: &Path) -> bool {
        pref_markers(dir).iter().all(|marker| marker.exists())
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nudge-diag-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for marker in pref_markers(&dir) {
            let _ = fs::remove_file(marker);
        }
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
        load_journal(dir).journal()
    }

    // beat once a second until `until`, answering every probe like a healthy main thread
    fn run_healthy(rec: &mut Recorder, from: u64, until: u64) {
        let mut t = from;
        while t <= until {
            if let Some(seq) = rec.tick(at(t)) {
                rec.counters.answered_probe.store(seq, Release);
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
        rec.counters.answered_probe.store(1, Release);
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
    fn deleting_the_folder_while_running_starts_a_fresh_launch_and_never_writes_the_old_one() {
        let dir = temp("deleted-while-running");
        let mut rec = recorder(&dir);
        run_healthy(&mut rec, 0, 10_000);
        assert!(rec.live);
        fs::remove_dir_all(&dir).unwrap();
        // the next beat finds the lock file gone, so this launch ends and a new one begins
        rec.tick(at(20_000));
        assert!(rec.live);
        let journal = disk(&dir);
        assert_eq!(journal.sessions.len(), 1, "only the new launch is recorded");
        assert_eq!(journal.sessions[0].end, End::Running);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deleting_the_folder_while_the_record_is_unusable_recovers_without_a_relaunch() {
        let dir = temp("deleted-unusable");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(JOURNAL), b"not a record").unwrap();
        let mut rec = recorder(&dir);
        assert_eq!(rec.tick(at(0)), None);
        assert!(!rec.live);
        assert_eq!(
            Status::from_u8(rec.counters.status.load(Relaxed)),
            Status::HistoryInvalid
        );
        // the lock is still held, so deleting the folder must not leave it stranded
        fs::remove_dir_all(&dir).unwrap();
        rec.tick(at(1_000));
        assert!(rec.live);
        let journal = disk(&dir);
        assert_eq!(journal.sessions.len(), 1, "a new launch is recorded");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deleting_only_the_lock_file_keeps_the_running_launch_and_its_counts() {
        let dir = temp("deleted-lock-only");
        let mut rec = recorder(&dir);
        run_healthy(&mut rec, 0, 10_000);
        let launch = rec.journal.sessions.last().unwrap().n;
        fs::remove_file(dir.join(LEASE)).unwrap();
        rec.tick(at(20_000));
        assert!(rec.live);
        let journal = disk(&dir);
        assert_eq!(journal.sessions.len(), 1, "the launch is continued, not restarted");
        assert_eq!(journal.sessions[0].n, launch);
        assert_eq!(journal.sessions[0].end, End::Running);
        assert!(
            !journal.events.iter().any(|e| e.kind == Kind::Lost),
            "a launch that is still running is not reported as lost"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_deleted_folder_starts_a_new_launch_without_the_old_events() {
        let dir = temp("deleted-events");
        let mut rec = recorder(&dir);
        run_healthy(&mut rec, 0, 10_000);
        let launch = rec.journal.sessions.last().unwrap().n;
        rec.journal.event(Kind::Stall, Phase::TrayMenu, 5);
        rec.dirty = true;
        rec.tick(at(10_500));
        fs::remove_dir_all(&dir).unwrap();
        rec.tick(at(20_000));
        assert!(rec.live);
        let journal = disk(&dir);
        assert_eq!(journal.sessions.len(), 1, "only the new launch is saved");
        assert_eq!(journal.sessions[0].end, End::Running);
        assert!(
            !journal.events.iter().any(|e| e.kind == Kind::Stall),
            "the old launch's events went with its folder"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_choice_that_cannot_be_saved_in_both_places_leaves_neither_behind() {
        let dir = temp("half-saved");
        let beside = pref_markers(&dir)[1].clone();
        // a folder where the second copy of the choice has to go makes its write fail
        fs::create_dir_all(&beside).unwrap();
        assert!(write_pref(&dir, false).is_err());
        assert!(!pref_markers(&dir)[0].exists(), "no half-saved Off is left to be read");
        fs::remove_dir_all(&beside).unwrap();
    }

    #[test]
    fn deleting_the_folder_does_not_turn_recording_back_on() {
        let dir = temp("deleted-while-off");
        fs::create_dir_all(&dir).unwrap();
        write_pref(&dir, false).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(read_pref(&dir), Pref::Off);
        // only turning it on, through the choice itself, lets recording start again
        write_pref(&dir, true).unwrap();
        assert_eq!(read_pref(&dir), Pref::On);
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
        // the probe sent before sleep is still the one waited for, so no second one is
        // queued; if nobody answers it by the end of the grace period plus 5 seconds, that is a real freeze
        let after = |t: u64| Clock {
            mono: t,
            wall: woke.wall + (t - 2_000),
        };
        assert_eq!(rec.tick(after(14_000)), None);
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
    fn a_stall_already_recorded_is_not_counted_again_after_a_sleep() {
        let dir = temp("stall-sleep");
        let mut rec = recorder(&dir);
        for t in (0..=6_000).step_by(1_000) {
            rec.tick(at(t));
        }
        assert_eq!(disk(&dir).sessions[0].stalls, 1);
        // the computer sleeps while the same probe is still unanswered
        let woke = Clock {
            mono: 7_000,
            wall: 1_000_000_000 + 7_000 + 600_000,
        };
        let after = |t: u64| Clock {
            mono: t,
            wall: woke.wall + (t - 7_000),
        };
        for t in (7_000..=30_000).step_by(1_000) {
            rec.tick(after(t));
        }
        assert_eq!(disk(&dir).sessions[0].stalls, 1, "one freeze, one stall");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stall_kept_across_a_sleep_reports_at_least_its_own_length_when_answered() {
        let dir = temp("stall-sleep-recovery");
        let mut rec = recorder(&dir);
        for t in (0..=6_000).step_by(1_000) {
            rec.tick(at(t));
        }
        let woke = Clock {
            mono: 7_000,
            wall: 1_000_000_000 + 7_000 + 600_000,
        };
        let after = |t: u64| Clock {
            mono: t,
            wall: woke.wall + (t - 7_000),
        };
        for t in (7_000..=20_000).step_by(1_000) {
            rec.tick(after(t));
        }
        rec.counters.answered_probe.store(1, Release);
        for t in (21_000..=23_000).step_by(1_000) {
            rec.tick(after(t));
        }
        let journal = disk(&dir);
        let recovered: Vec<_> = journal
            .events
            .iter()
            .filter(|e| e.kind == Kind::Recovered)
            .collect();
        assert_eq!(recovered.len(), 1, "one answer, one recovery");
        // the first probe went out at 1 s and was answered at 21 s: 20 s of waiting,
        // counting the freeze before the sleep. the sleep does not move the monotonic clock
        assert_eq!(recovered[0].secs, 20);
        assert_eq!(journal.sessions[0].stalls, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_freeze_after_a_real_recovery_is_counted_as_a_second_stall() {
        let dir = temp("second-freeze");
        let mut rec = recorder(&dir);
        for t in (0..=6_000).step_by(1_000) {
            rec.tick(at(t));
        }
        rec.counters.answered_probe.store(1, Release);
        for t in (7_000..=8_000).step_by(1_000) {
            rec.tick(at(t));
        }
        assert_eq!(disk(&dir).sessions[0].stalls, 1);
        // answered, then frozen again with no sleep in between
        for t in (9_000..=16_000).step_by(1_000) {
            rec.tick(at(t));
        }
        let journal = disk(&dir);
        assert_eq!(journal.sessions[0].stalls, 2);
        assert_eq!(
            journal
                .events
                .iter()
                .filter(|e| e.kind == Kind::Recovered)
                .count(),
            1
        );
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
            None,
            "the probe from before is still the one waited for, not a second one"
        );
        rec.tick(at(63_000));
        rec.tick(at(66_000));
        assert_eq!(disk(&dir).sessions.iter().map(|s| s.stalls).sum::<u32>(), 0);
        // its wait began again when recording did, so it is a freeze 5 seconds after that
        rec.tick(at(67_000));
        assert_eq!(disk(&dir).sessions.iter().map(|s| s.stalls).sum::<u32>(), 1);
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
        // the choice outlives the folder, so clear it to start the next case clean
        write_pref(&dir, true).unwrap();

        // a file where the folder should be: the marker can be neither found nor ruled out
        fs::write(&dir, "not a folder").unwrap();
        assert_eq!(read_pref(&dir), Pref::Unreadable);
        let mut rec = recorder(&dir);
        assert_eq!(rec.tick(at(0)), None);
        assert_eq!(
            Status::from_u8(rec.counters.status.load(Relaxed)),
            Status::PrefUnreadable
        );
        assert!(!rec.counters.enabled());
        fs::remove_file(&dir).unwrap();
    }

    #[test]
    fn a_choice_that_cannot_be_saved_still_stops_recording() {
        let dir = temp("unsaved");
        fs::write(&dir, "in the way").unwrap();
        let c = counters();
        apply_choice(Some(&dir), c, false);
        assert!(!c.enabled());
        assert!(c.off_until_quit.load(Relaxed));
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
        assert!(!c.enabled(), "on counts only once it is saved");
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
        assert!(off_on_disk(&ok) && !c.enabled() && !c.off_until_quit.load(Relaxed));
        apply_choice(Some(&ok), c, true);
        assert!(!off_on_disk(&ok) && c.enabled());
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
            .any(|e| e.kind == Kind::Lost && e.launch == journal.sessions[1].n));
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
            journal
                .events
                .iter()
                .all(|e| e.launch >= journal.sessions[0].n),
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
        assert!(
            render_report(&journal, false, "1.0", None).contains(&format!("Panics: {}", t.panics))
        );
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
        let Load::Valid(journal) = load_journal(&dir) else {
            panic!("a file that parses is read, then held to the limits");
        };
        assert!(journal.sessions.iter().all(|s| s.slow.len() == SLOTS));
        let text = render_report(&journal, true, "1.0.7", None);
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

        // too big to be ours: left exactly as it is, and recording does not start over it
        let big = format!("{}{}", " ".repeat(MAX_JOURNAL_BYTES as usize), r#"{"v":1}"#);
        fs::write(dir.join(JOURNAL), &big).unwrap();
        assert_eq!(load_journal(&dir), Load::Invalid);
        let mut rec = recorder(&dir);
        assert_eq!(rec.tick(at(0)), None);
        assert!(!rec.live);
        assert_eq!(
            Status::from_u8(rec.counters.status.load(Relaxed)),
            Status::HistoryInvalid
        );
        assert_eq!(fs::read(dir.join(JOURNAL)).unwrap(), big.as_bytes());
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [JOURNAL, LEASE],
            "no copy, no rename, no temporary file"
        );

        // not JSON, and a format from some other version, are not read either
        for text in [
            "{ not json",
            r#"{"v":2,"sessions":[]}"#,
            "",
            r#"{"sessions":[]}"#,
        ] {
            fs::write(dir.join(JOURNAL), text).unwrap();
            assert_eq!(load_journal(&dir), Load::Invalid, "{text:?}");
        }
        // deep nesting is refused by the parser, not recursed into
        fs::write(
            dir.join(JOURNAL),
            format!("{}{}", "[".repeat(5000), "]".repeat(5000)),
        )
        .unwrap();
        assert_eq!(load_journal(&dir), Load::Invalid);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(load_journal(&dir), Load::Missing);
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
        COUNTERS.choose(true);
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
        COUNTERS.choose(false);
        {
            let _off = span(Phase::ConfigSave);
            std::thread::sleep(Duration::from_millis(130));
        }
        assert_eq!(
            COUNTERS.slow[index].load(Relaxed),
            before + 1,
            "nothing counted while off"
        );
        COUNTERS.choose(true);
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
        let text = render_report(&disk(&dir), true, "1.2.3", None);
        assert!(text.contains("- opening Settings: 1 to 5 s x2"), "{text}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_panic_leaves_a_count_and_never_its_message() {
        let _guard = GLOBAL.lock().unwrap();
        let dir = temp("panic");
        let secret = "SENTINEL-alice /Users/alice/reminder-call-mum.txt";
        install_panic_hook();
        COUNTERS.choose(true);
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
        COUNTERS.choose(false);
        let _ = std::thread::spawn(|| panic!("while off")).join();
        assert_eq!(
            COUNTERS.panics.load(Relaxed),
            before + 2,
            "a panic while off is not counted"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    fn report_of(dir: &Path) -> String {
        render_report(&load_journal(dir).journal(), true, "1.2.3", None)
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
        let t = totals(&load_journal(&temp("nothing")).journal());
        assert_eq!((t.panics, t.stalls, t.unclean, t.launches), (0, 0, 0, 0));
        let text = render_report(&Journal::fresh(), false, "1.0.7", None);
        assert!(text.contains("Recording now: no") && text.contains("Panics: 0"));
    }

    // runs one beat on another thread and holds it just before it commits a
    // write; `during` runs while it is held
    fn held_beat(mut rec: Recorder, during: impl FnOnce()) -> Recorder {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        rec.gate = Some(Box::new(move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        }));
        let beat = std::thread::spawn(move || {
            rec.tick(at(1_000));
            rec
        });
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the beat reached its write");
        during();
        drop(release_tx);
        let mut rec = beat.join().unwrap();
        rec.gate = None;
        rec
    }

    // a path where a file stands in the way of the preference file
    fn unsavable(name: &str) -> PathBuf {
        let path = temp(name);
        fs::write(&path, "in the way").unwrap();
        path
    }

    #[test]
    fn an_off_choice_made_while_a_launch_starts_stops_it_from_starting() {
        for saved in [true, false] {
            let dir = temp("start-off");
            let blocked = unsavable("start-off-elsewhere");
            let c = counters();
            let rec = held_beat(Recorder::new(dir.clone(), c, "1.2.3"), || {
                apply_choice(Some(if saved { &dir } else { &blocked }), c, false)
            });
            assert!(!rec.live && !c.enabled());
            assert_eq!(
                load_journal(&dir),
                Load::Missing,
                "no launch was written (saved: {saved})"
            );
            assert_eq!(Status::shown(c), Status::Off, "not the stale Recording");
            assert_eq!(c.problem.load(Relaxed) == PROBLEM_STOP_UNSAVED, !saved);

            // and it stays that way on the beats after, saved or not
            let mut rec = rec;
            for t in [2_000, 3_000] {
                assert_eq!(rec.tick(at(t)), None);
            }
            assert!(!rec.live);
            assert_eq!(load_journal(&dir), Load::Missing);
            assert_eq!(Status::shown(c), Status::Off);
            assert_eq!(off_on_disk(&dir), saved);

            if saved {
                apply_choice(Some(&dir), c, true);
                rec.tick(at(4_000));
                assert!(rec.live);
                assert_eq!(Status::shown(c), Status::Recording);
                assert_eq!(disk(&dir).sessions.len(), 1);
            }
            fs::remove_dir_all(&dir).unwrap();
            let _ = fs::remove_file(&blocked);
        }
    }

    #[test]
    fn an_off_choice_made_during_a_record_write_is_not_undone_by_it() {
        for saved in [true, false] {
            let dir = temp("write-off");
            let blocked = unsavable("write-off-elsewhere");
            let c = counters();
            let mut rec = Recorder::new(dir.clone(), c, "1.2.3");
            rec.tick(at(0));
            let before = fs::read(dir.join(JOURNAL)).unwrap();
            // a stall noticed in memory, waiting for its write
            rec.journal.sessions.last_mut().unwrap().stalls = 1;
            rec.journal.event(Kind::Stall, Phase::TrayMenu, 5);
            rec.dirty = true;

            let mut rec = held_beat(rec, || {
                apply_choice(Some(if saved { &dir } else { &blocked }), c, false)
            });
            assert_eq!(
                fs::read(dir.join(JOURNAL)).unwrap(),
                before,
                "nothing new is written once off was chosen (saved: {saved})"
            );
            assert!(rec.dirty);
            assert_eq!(Status::shown(c), Status::Off);

            // the next beat closes the launch with what was gathered before off
            assert_eq!(rec.tick(at(2_000)), None);
            let journal = disk(&dir);
            assert_eq!(journal.sessions[0].end, End::Stopped);
            assert_eq!(journal.sessions[0].stalls, 1);
            assert_eq!(Status::shown(c), Status::Off);
            assert!(Lease::acquire(&dir).is_ok(), "the lock was let go");
            fs::remove_dir_all(&dir).unwrap();
            let _ = fs::remove_file(&blocked);
        }
    }

    #[test]
    fn a_reading_taken_before_a_choice_cannot_undo_it() {
        let c = counters();
        let before = c.policy.load(Acquire);
        c.choose(false);
        assert_eq!(c.adopt(before, true), None, "the stale reading is dropped");
        assert!(!c.enabled());
        let now = c.policy.load(Acquire);
        assert!(c.adopt(now, true).is_some(), "a current reading counts");
        assert!(c.enabled());
        c.choose(false);
        assert!(!c.enabled());
        c.choose(true);
        assert!(c.enabled());
    }

    #[test]
    fn a_record_that_cannot_be_read_is_left_alone_and_recording_waits() {
        let dir = temp("unreadable-history");
        let mut first = recorder(&dir);
        run_healthy(&mut first, 0, 1_000);
        first.finish_clean().unwrap();
        drop(first);
        let good = fs::read(dir.join(JOURNAL)).unwrap();

        // a folder where the file belongs: it opens, and cannot be read
        fs::rename(dir.join(JOURNAL), dir.join("kept")).unwrap();
        fs::create_dir(dir.join(JOURNAL)).unwrap();
        assert_eq!(load_journal(&dir), Load::Unreadable);
        let mut rec = recorder(&dir);
        assert_eq!(rec.tick(at(0)), None);
        assert!(!rec.live);
        assert_eq!(
            Status::from_u8(rec.counters.status.load(Relaxed)),
            Status::HistoryUnreadable
        );
        assert!(dir.join(JOURNAL).is_dir(), "not renamed, not replaced");
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, [JOURNAL, "kept", LEASE]);

        // once it can be read again, recording starts and the earlier launch is still there
        fs::remove_dir(dir.join(JOURNAL)).unwrap();
        fs::rename(dir.join("kept"), dir.join(JOURNAL)).unwrap();
        rec.tick(at(1_000));
        assert!(rec.live);
        let journal = disk(&dir);
        assert_eq!(
            journal.sessions.iter().map(|s| s.end).collect::<Vec<_>>(),
            vec![End::Clean, End::Running]
        );
        drop(rec);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.join(JOURNAL);
            let saved = fs::read(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            // a user who can read anyway (root) cannot show this
            if File::open(&path).is_err() {
                assert_eq!(load_journal(&dir), Load::Unreadable);
                let mut rec = recorder(&dir);
                assert_eq!(rec.tick(at(0)), None);
                assert_eq!(
                    Status::from_u8(rec.counters.status.load(Relaxed)),
                    Status::HistoryUnreadable
                );
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                assert_eq!(fs::read(&path).unwrap(), saved, "the record is as it was");
                assert!(!dir.join("journal.bad").exists());
                rec.tick(at(1_000));
                assert!(rec.live);
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_ne!(good.len(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_report_over_an_unusable_record_says_so_instead_of_showing_zeros_alone() {
        let text = render_report(
            &Journal::fresh(),
            false,
            "1.0.7",
            Some("could not be read, so the counts below are empty"),
        );
        assert!(text.contains("Saved record: could not be read, so the counts below are empty"));
        assert!(!render_report(&Journal::fresh(), false, "1.0.7", None).contains("Saved record"));
    }

    #[test]
    fn only_plain_version_numbers_from_a_file_reach_the_report() {
        let dir = temp("builds");
        fs::create_dir_all(&dir).unwrap();
        let raw = r#"{"v":1,"next":6,"sessions":[
            {"n":1,"build":"Alice-Smith","end":"Clean","panics":2},
            {"n":2,"build":"PRIVATEIDENTITY123","end":"Lost","stalls":3},
            {"n":3,"build":"1.0.7-Alice","end":"Clean"},
            {"n":4,"build":"1.0.7","end":"Clean"},
            {"n":5,"build":"1.0","end":"Clean"}],
            "events":[{"launch":2,"kind":"Stall","phase":"TrayMenu","secs":5}]}"#;
        fs::write(dir.join(JOURNAL), raw).unwrap();
        let journal = disk(&dir);
        let text = render_report(&journal, true, "PRIVATEIDENTITY123", None);
        for personal in ["Alice", "Smith", "PRIVATEIDENTITY123", "1.0.7-", "1.0\n"] {
            assert!(
                !text.contains(personal),
                "{personal:?} reached the report:\n{text}"
            );
        }
        assert!(text.contains("Nudge version: unknown"));
        assert!(text.contains("Versions: unknown, 1.0.7"), "{text}");
        // the history is still all there
        let t = totals(&journal);
        assert_eq!((t.panics, t.stalls, t.unclean, t.launches), (2, 3, 1, 5));
        assert!(text.contains("UI stall (step: updating the tray menu)"));

        assert_eq!(
            clean_build(env!("CARGO_PKG_VERSION")),
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(clean_build("12.0.345"), "12.0.345");
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.2.3-beta",
            "a.b.c",
            "1.2.3 ",
            "1..3",
            "123456.1.1",
            "+1.2.3",
            "\u{661}.\u{662}.\u{663}",
        ] {
            assert_eq!(clean_build(bad), "unknown", "{bad:?}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hang_after_quitting_began_is_named_as_quitting() {
        let dir = temp("quit-phase");
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        rec.counters.phase.store(Phase::TrayMenu as u8, Relaxed);
        rec.counters.quitting.store(true, Relaxed);
        for t in 1..=5 {
            rec.tick(at(t * 1_000));
        }
        let journal = disk(&dir);
        let stall = journal.events.last().unwrap();
        assert_eq!((stall.kind, stall.phase), (Kind::Stall, Phase::Quit));
        assert_eq!(Phase::Quit.label(), "quitting");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_closing_write_that_fails_keeps_the_launch_and_collects_nothing_new() {
        let dir = temp("closing");
        let c = counters();
        let mut rec = Recorder::new(dir.clone(), c, "1.2.3");
        run_healthy(&mut rec, 0, 2_000);
        let step = slot(Phase::SettingsOpen, 2).unwrap();
        c.slow[step].store(2, Relaxed);
        c.panics.store(1, Relaxed);
        fs::create_dir(dir.join(JOURNAL_TMP)).unwrap();

        // the choice could not be saved, so a second copy still reads "on"
        let blocked = unsavable("closing-elsewhere");
        apply_choice(Some(&blocked), c, false);
        assert_eq!(rec.tick(at(3_000)), None);
        assert_eq!(
            Status::from_u8(c.status.load(Relaxed)),
            Status::ClosePending,
            "not reported as plain Off"
        );
        let view = make_view(&Journal::fresh(), c);
        assert_eq!(
            (view.status, view.problem),
            ("close_pending", Some("stop_unsaved")),
            "both the save of the choice and the closing write failed, and both are said"
        );
        assert!(rec.live && rec.closing && rec.lease.is_some());
        assert!(rec.finish_clean().is_err(), "quitting reports it too");

        // nothing new is gathered once off, and another copy cannot take over a live owner
        c.slow[step].store(50, Relaxed);
        c.panics.store(9, Relaxed);
        let mut second = recorder(&dir);
        assert_eq!(second.tick(at(0)), None);
        assert_eq!(
            Status::from_u8(second.counters.status.load(Relaxed)),
            Status::InUse
        );
        assert_eq!(disk(&dir).sessions[0].end, End::Running);
        assert_eq!(totals(&disk(&dir)).unclean, 0);

        // tried again on a slow cadence, not every beat
        fs::remove_dir(dir.join(JOURNAL_TMP)).unwrap();
        rec.tick(at(4_000));
        assert_eq!(disk(&dir).sessions[0].end, End::Running);
        rec.tick(at(3_000 + CLOSE_RETRY_MS));
        let closed = disk(&dir);
        assert_eq!(closed.sessions[0].end, End::Stopped);
        assert_eq!(
            (closed.sessions[0].slow[step], closed.sessions[0].panics),
            (2, 1),
            "what was gathered before off, and no more"
        );
        assert_eq!(Status::shown(c), Status::Off);
        assert!(!rec.live && rec.lease.is_none());

        // the next owner finds a closed launch, not a lost one
        second.tick(at(20_000));
        assert!(second.live);
        assert_eq!(
            disk(&dir)
                .sessions
                .iter()
                .map(|s| s.end)
                .collect::<Vec<_>>(),
            vec![End::Stopped, End::Running]
        );
        assert_eq!(totals(&disk(&dir)).unclean, 0);
        fs::remove_dir_all(&dir).unwrap();
        let _ = fs::remove_file(&blocked);
    }

    #[test]
    fn a_pending_close_is_finished_before_a_new_launch_starts() {
        let dir = temp("closing-then-on");
        let mut rec = recorder(&dir);
        rec.tick(at(0));
        fs::create_dir(dir.join(JOURNAL_TMP)).unwrap();
        write_pref(&dir, false).unwrap();
        rec.tick(at(1_000));
        let view = make_view(&Journal::fresh(), rec.counters);
        assert_eq!(
            (view.keep, view.status, view.problem),
            (false, "close_pending", None),
            "the choice was saved; only the closing write is outstanding"
        );
        write_pref(&dir, true).unwrap();
        // on again, but the old launch still cannot be closed: nothing new starts
        assert_eq!(rec.tick(at(2_000)), None);
        assert_eq!(disk(&dir).sessions.len(), 1);
        fs::remove_dir(dir.join(JOURNAL_TMP)).unwrap();
        rec.tick(at(1_000 + CLOSE_RETRY_MS));
        let journal = disk(&dir);
        assert_eq!(
            journal.sessions.iter().map(|s| s.end).collect::<Vec<_>>(),
            vec![End::Stopped, End::Running]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn one_probe_stays_in_flight_across_a_reset_and_a_sleep() {
        // nobody ever answers; `seq` counts the probes posted
        fn run(watch: &mut Watch, now: Clock) -> Seen {
            watch.step(now, 0, Phase::TrayMenu).1
        }
        let mut watch = Watch::default();
        assert_eq!(run(&mut watch, at(0)), Seen::Nothing);
        assert_eq!(watch.seq, 1);

        watch.reset();
        assert_eq!(run(&mut watch, at(1_000)), Seen::Nothing);
        // the computer sleeps, then the grace period passes
        let woke = Clock {
            mono: 2_000,
            wall: 1_000_000_000 + 2_000 + 600_000,
        };
        assert_eq!(run(&mut watch, woke), Seen::Nothing);
        let later = |t: u64| Clock {
            mono: t,
            wall: woke.wall + (t - 2_000),
        };
        for t in (3_000..=18_000).step_by(1_000) {
            assert_eq!(run(&mut watch, later(t)), Seen::Nothing, "at {t}");
        }
        watch.reset();
        assert_eq!(run(&mut watch, later(19_000)), Seen::Nothing);
        assert_eq!(
            watch.seq, 1,
            "no second probe while the first is unanswered"
        );

        // still unanswered 5 seconds on: the freeze is found, on the same probe
        for t in (20_000..=23_000).step_by(1_000) {
            assert_eq!(run(&mut watch, later(t)), Seen::Nothing, "at {t}");
        }
        assert_eq!(
            run(&mut watch, later(24_000)),
            Seen::Stalled(Phase::TrayMenu)
        );
        assert_eq!(watch.seq, 1);

        // answered at last: one recovery, then the next probe
        let (post, seen) = watch.step(later(25_000), 1, Phase::Idle);
        assert_eq!(post, Some(2));
        assert!(matches!(
            seen,
            Seen::Recovered {
                phase: Phase::TrayMenu,
                ..
            }
        ));
    }
}
