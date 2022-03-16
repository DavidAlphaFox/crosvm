// Copyright 2022 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Facilities for sending log message to syslog.
//!
//! Every function exported by this module is thread-safe. Each function will silently fail until
//! `syslog::init()` is called and returns `Ok`.
//!
//! # Examples
//!
//! ```
//! use win_sys_util::{error, syslog, warn};
//!
//! if let Err(e) = syslog::init() {
//!     println!("failed to initiailize syslog: {}", e);
//!     return;
//! }
//! warn!("this is your {} warning", "final");
//! error!("something went horribly wrong: {}", "out of RAMs");
//! ```

pub use super::win::syslog::PlatformSyslog;
use super::{syslog_lock, AsRawDescriptor, RawDescriptor, CHRONO_TIMESTAMP_FIXED_FMT};
use serde::{Deserialize, Serialize};
use std::{
    convert::{From, Into, TryFrom},
    env,
    ffi::{OsStr, OsString},
    fmt::{
        Display, {self},
    },
    fs::File,
    io,
    io::{stderr, Cursor, Write},
    path::{Path, PathBuf},
    sync::{MutexGuard, Once},
};

use remain::sorted;
use sync::Mutex;
use thiserror::Error as ThisError;

/// The priority (i.e. severity) of a syslog message.
///
/// See syslog man pages for information on their semantics.
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Priority {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
}

impl Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Priority::*;

        let string = match self {
            Emergency => "EMERGENCY",
            Alert => "ALERT",
            Critical => "CRITICAL",
            Error => "ERROR",
            Warning => "WARNING",
            Notice => "NOTICE",
            Info => "INFO",
            Debug => "DEBUG",
        };

        write!(f, "{}", string)
    }
}

impl TryFrom<&str> for Priority {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, <Self as TryFrom<&str>>::Error> {
        match value {
            "0" | "EMERGENCY" => Ok(Priority::Emergency),
            "1" | "ALERT" => Ok(Priority::Alert),
            "2" | "CRITICAL" => Ok(Priority::Critical),
            "3" | "ERROR" => Ok(Priority::Error),
            "4" | "WARNING" => Ok(Priority::Warning),
            "5" | "NOTICE" => Ok(Priority::Notice),
            "6" | "INFO" => Ok(Priority::Info),
            "7" | "DEBUG" => Ok(Priority::Debug),
            _ => Err("Priority can only be parsed from 0-7 and given variant names"),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PriorityFilter {
    Silent,
    Priority(Priority),
    ShowAll,
}

impl From<Priority> for PriorityFilter {
    fn from(pri: Priority) -> Self {
        PriorityFilter::Priority(pri)
    }
}

impl TryFrom<&str> for PriorityFilter {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.to_uppercase().as_str() {
            "S" | "SILENT" => Ok(PriorityFilter::Silent),
            "*" => Ok(PriorityFilter::ShowAll),
            value => match Priority::try_from(value) {
                Ok(pri) => Ok(PriorityFilter::Priority(pri)),
                Err(_) => Err("PriorityFilter can only be parsed from valid Priority
                value, S, *, or SILENT"),
            },
        }
    }
}

/// The facility of a syslog message.
///
/// See syslog man pages for information on their semantics.
#[derive(Copy, Clone)]
pub enum Facility {
    Kernel = 0,
    User = 1 << 3,
    Mail = 2 << 3,
    Daemon = 3 << 3,
    Auth = 4 << 3,
    Syslog = 5 << 3,
    Lpr = 6 << 3,
    News = 7 << 3,
    Uucp = 8 << 3,
    Local0 = 16 << 3,
    Local1 = 17 << 3,
    Local2 = 18 << 3,
    Local3 = 19 << 3,
    Local4 = 20 << 3,
    Local5 = 21 << 3,
    Local6 = 22 << 3,
    Local7 = 23 << 3,
}

/// Errors returned by `syslog::init()`.
#[sorted]
#[derive(ThisError, Debug)]
pub enum Error {
    /// Error while attempting to connect socket.
    #[error("failed to connect socket: {0}")]
    Connect(io::Error),
    /// There was an error using `open` to get the lowest file descriptor.
    #[error("failed to get lowest file descriptor: {0}")]
    GetLowestFd(io::Error),
    /// The guess of libc's file descriptor for the syslog connection was invalid.
    #[error("guess of fd for syslog connection was invalid")]
    InvalidFd,
    /// Initialization was never attempted.
    #[error("initialization was never attempted")]
    NeverInitialized,
    /// Initialization has previously failed and can not be retried.
    #[error("initialization previously failed and cannot be retried")]
    Poisoned,
    /// Error while creating socket.
    #[error("failed to create socket: {0}")]
    Socket(io::Error),
}

/// Defines a log level override for a set of source files with the given path_prefix
#[derive(Debug)]
struct PathFilter {
    path_prefix: PathBuf,
    level: PriorityFilter,
}

fn get_proc_name() -> Option<String> {
    env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|s| s.file_name().map(OsStr::to_os_string))
        .map(OsString::into_string)
        .and_then(Result::ok)
}

struct State {
    stderr: bool,
    file: Option<File>,
    proc_name: Option<String>,
    syslog: PlatformSyslog,
    log_level: PriorityFilter, // This is the default global log level
    path_log_levels: Vec<PathFilter>, // These are sorted with longest path prefixes first
}

impl State {
    fn new() -> Result<State, Error> {
        Ok(State {
            stderr: true,
            file: None,
            proc_name: get_proc_name(),
            syslog: PlatformSyslog::new()?,
            log_level: PriorityFilter::Priority(Priority::Info),
            path_log_levels: Vec::new(),
        })
    }
}

/// Set the log level filter.
///
/// Does nothing if syslog was never initialized. Set log level filter with the given priority filter level.
pub fn set_log_level<T: Into<PriorityFilter>>(log_level: T) {
    let mut state = syslog_lock!();
    state.log_level = log_level.into();
}

/// Adds a new per-path log level filter.
pub fn add_path_log_level<T: Into<PriorityFilter>>(path_prefix: &str, log_level: T) {
    // Insert filter so that path_log_levels is always sorted with longer prefixes first
    let mut state = syslog_lock!();
    let index = state
        .path_log_levels
        .binary_search_by_key(&path_prefix.len(), |p| {
            std::usize::MAX - p.path_prefix.as_os_str().len()
        })
        .unwrap_or_else(|e| e);
    state.path_log_levels.insert(
        index,
        PathFilter {
            path_prefix: PathBuf::from(path_prefix),
            level: log_level.into(),
        },
    );
}

static STATE_ONCE: Once = Once::new();
static mut STATE: *const Mutex<State> = 0 as *const _;

fn new_mutex_ptr<T>(inner: T) -> *const Mutex<T> {
    Box::into_raw(Box::new(Mutex::new(inner)))
}

/// Initialize the syslog connection and internal variables.
///
/// This should only be called once per process before any other threads have been spawned or any
/// signal handlers have been registered. Every call made after the first will have no effect
/// besides return `Ok` or `Err` appropriately.
pub fn init() -> Result<(), Error> {
    let mut err = Error::Poisoned;
    STATE_ONCE.call_once(|| match State::new() {
        // Safe because STATE mutation is guarded by `Once`.
        Ok(state) => unsafe { STATE = new_mutex_ptr(state) },
        Err(e) => err = e,
    });

    if unsafe { STATE.is_null() } {
        Err(err)
    } else {
        Ok(())
    }
}

fn lock() -> Result<MutexGuard<'static, State>, Error> {
    // Safe because we assume that STATE is always in either a valid or NULL state.
    let state_ptr = unsafe { STATE };
    if state_ptr.is_null() {
        return Err(Error::NeverInitialized);
    }
    // Safe because STATE only mutates once and we checked for NULL.
    let state = unsafe { &*state_ptr };
    let guard = state.lock();
    Ok(guard)
}

// Attempts to lock and retrieve the state. Returns from the function silently on failure.
#[macro_export]
macro_rules! syslog_lock {
    () => {
        match lock() {
            Ok(s) => s,
            _ => return,
        }
    };
}

pub fn log_enabled(pri: Priority, file_path: &str) -> Option<bool> {
    let log_level = match lock() {
        Ok(state) => {
            let parsed_path = Path::new(file_path);
            // Since path_log_levels is sorted with longest prefixes first, this will yield the
            // longest matching prefix if one exists.
            state
                .path_log_levels
                .iter()
                .find_map(|filter| {
                    if parsed_path.starts_with(filter.path_prefix.as_path()) {
                        Some(filter.level)
                    } else {
                        None
                    }
                })
                .unwrap_or(state.log_level)
        }
        _ => return None,
    };
    match log_level {
        PriorityFilter::ShowAll => Some(true),
        PriorityFilter::Silent => Some(false),
        PriorityFilter::Priority(log_level) => Some((pri as u8) <= (log_level as u8)),
    }
}

/// A macro for logging at an arbitrary priority level.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! log {
    ($pri:expr, $($args:tt)+) => ({
        if let Some(true) = $crate::syslog::log_enabled($pri, file!()) {
            $crate::syslog::log($pri, $crate::syslog::Facility::User, Some((file!(), line!())), format_args!($($args)+))
        }
    })
}

/// Replaces the process name reported in each syslog message.
///
/// The default process name is the _file name_ of `argv[0]`. For example, if this program was
/// invoked as
///
/// ```bash
/// $ path/to/app --delete everything
/// ```
///
/// the default process name would be _app_.
///
/// Does nothing if syslog was never initialized.
pub fn set_proc_name<T: Into<String>>(proc_name: T) {
    let mut state = syslog_lock!();
    state.proc_name = Some(proc_name.into());
}

pub(crate) trait Syslog {
    fn new() -> Result<Self, Error>
    where
        Self: Sized;

    /// Enables or disables echoing log messages to the syslog.
    ///
    /// The default behavior is **enabled**.
    ///
    /// If `enable` goes from `true` to `false`, the syslog connection is closed. The connection is
    /// reopened if `enable` is set to `true` after it became `false`.
    ///
    /// Returns an error if syslog was never initialized or the syslog connection failed to be
    /// established.
    ///
    /// # Arguments
    /// * `enable` - `true` to enable echoing to syslog, `false` to disable echoing to syslog.
    fn enable(&mut self, enable: bool) -> Result<(), Error>;

    fn log(
        &self,
        proc_name: Option<&str>,
        pri: Priority,
        fac: Facility,
        file_line: Option<(&str, u32)>,
        args: fmt::Arguments,
    );

    fn push_descriptors(&self, fds: &mut Vec<RawDescriptor>);
}

/// Enables or disables echoing log messages to the syslog.
///
/// The default behavior is **enabled**.
///
/// If `enable` goes from `true` to `false`, the syslog connection is closed. The connection is
/// reopened if `enable` is set to `true` after it became `false`.
///
/// Returns an error if syslog was never initialized or the syslog connection failed to be
/// established.
///
/// # Arguments
/// * `enable` - `true` to enable echoing to syslog, `false` to disable echoing to syslog.
pub fn echo_syslog(enable: bool) -> Result<(), Error> {
    let state_ptr = unsafe { STATE };
    if state_ptr.is_null() {
        return Err(Error::NeverInitialized);
    }
    let mut state = lock().map_err(|_| Error::Poisoned)?;

    state.syslog.enable(enable)
}

/// Replaces the optional `File` to echo log messages to.
///
/// The default behavior is to not echo to a file. Passing `None` to this function restores that
/// behavior.
///
/// Does nothing if syslog was never initialized.
///
/// # Arguments
/// * `file` - `Some(file)` to echo to `file`, `None` to disable echoing to the file previously passed to `echo_file`.
pub fn echo_file(file: Option<File>) {
    let mut state = syslog_lock!();
    state.file = file;
}

/// Enables or disables echoing log messages to the `std::io::stderr()`.
///
/// The default behavior is **enabled**.
///
/// Does nothing if syslog was never initialized.
///
/// # Arguments
/// * `enable` - `true` to enable echoing to stderr, `false` to disable echoing to stderr.
pub fn echo_stderr(enable: bool) {
    let mut state = syslog_lock!();
    state.stderr = enable;
}

/// Retrieves the file descriptors owned by the global syslogger.
///
/// Does nothing if syslog was never initialized. If their are any file descriptors, they will be
/// pushed into `fds`.
///
/// Note that the `stderr` file descriptor is never added, as it is not owned by syslog.
pub fn push_descriptors(fds: &mut Vec<RawDescriptor>) {
    let state = syslog_lock!();
    state.syslog.push_descriptors(fds);
    fds.extend(state.file.iter().map(|f| f.as_raw_descriptor()));
}

/// Compatability function for callers using the old naming
pub fn push_fds(fds: &mut Vec<RawDescriptor>) {
    push_descriptors(fds)
}

/// Records a log message with the given details.
///
/// Note that this will fail silently if syslog was not initialized.
///
/// # Arguments
/// * `pri` - The `Priority` (i.e. severity) of the log message.
/// * `fac` - The `Facility` of the log message. Usually `Facility::User` should be used.
/// * `file_line` - Optional tuple of the name of the file that generated the
///                 log and the line number within that file.
/// * `args` - The log's message to record, in the form of `format_args!()`  return value
///
/// # Examples
///
/// ```
/// # use win_sys_util::syslog;
/// # if let Err(e) = syslog::init() {
/// #     println!("failed to initiailize syslog: {}", e);
/// #     return;
/// # }
/// syslog::log(syslog::Priority::Error,
///             syslog::Facility::User,
///             Some((file!(), line!())),
///             format_args!("hello syslog"));
/// ```
pub fn log(pri: Priority, fac: Facility, file_line: Option<(&str, u32)>, args: fmt::Arguments) {
    let mut state = syslog_lock!();
    let mut buf = [0u8; 2048];

    state.syslog.log(
        state.proc_name.as_ref().map(|s| s.as_ref()),
        pri,
        fac,
        file_line,
        args,
    );

    let res = {
        let mut buf_cursor = Cursor::new(&mut buf[..]);
        let now = chrono::Local::now()
            .format(CHRONO_TIMESTAMP_FIXED_FMT!())
            .to_string();
        if let Some((file_name, line)) = &file_line {
            write!(&mut buf_cursor, "[{}:{}:{}:{}] ", now, pri, file_name, line)
        } else {
            write!(&mut buf_cursor, "[{}]", now)
        }
        .and_then(|()| writeln!(&mut buf_cursor, "{}", args))
        .map(|()| buf_cursor.position() as usize)
    };
    if let Ok(len) = &res {
        write_to_file(&buf, *len, &mut state);
    } else if let Err(e) = &res {
        // Don't use warn macro to avoid potential recursion issues after macro expansion.
        let err_buf = [0u8; 1024];
        let mut buf_cursor = Cursor::new(&mut buf[..]);
        if writeln!(
            &mut buf_cursor,
            "[{}]:WARNING: Failed to log with err: {:?}",
            chrono::Local::now().format(CHRONO_TIMESTAMP_FIXED_FMT!()),
            e
        )
        .is_ok()
        {
            write_to_file(&err_buf, buf_cursor.position() as usize, &mut state);
        }
    }
}

fn write_to_file(buf: &[u8], len: usize, state: &mut State) {
    if let Some(file) = state.file.as_mut() {
        let _ = file.write_all(&buf[..len]);
    }
    if state.stderr {
        let _ = stderr().write_all(&buf[..len]);
    }
}

/// A macro for logging an error.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! error {
    ($($args:tt)+) => ($crate::log!($crate::syslog::Priority::Error, $($args)*))
}

/// A macro for logging a warning.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! warn {
    ($($args:tt)+) => ($crate::log!($crate::syslog::Priority::Warning, $($args)*))
}

/// A macro for logging info.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! info {
    ($($args:tt)+) => ($crate::log!($crate::syslog::Priority::Info, $($args)*))
}

/// A macro for logging debug information.
///
/// Note that this will fail silently if syslog was not initialized.
#[macro_export]
macro_rules! debug {
    ($($args:tt)+) => ($crate::log!($crate::syslog::Priority::Debug, $($args)*))
}

// Struct that implements io::Write to be used for writing directly to the syslog
pub struct Syslogger {
    buf: String,
    priority: Priority,
    facility: Facility,
}

impl Syslogger {
    pub fn new(p: Priority, f: Facility) -> Syslogger {
        Syslogger {
            buf: String::new(),
            priority: p,
            facility: f,
        }
    }
}

impl io::Write for Syslogger {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let parsed_str = String::from_utf8_lossy(buf);
        self.buf.push_str(&parsed_str);

        if let Some(last_newline_idx) = self.buf.rfind('\n') {
            for line in self.buf[..last_newline_idx].lines() {
                log(self.priority, self.facility, None, format_args!("{}", line));
            }

            self.buf.drain(..=last_newline_idx);
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// TODO(b/223733375): Enable ignored flaky tests.
#[cfg(test)]
mod tests {
    use super::*;

    use super::super::{BlockingMode, FramingMode, StreamChannel};
    use regex::Regex;
    use std::{convert::TryInto, io::Read, os::windows::io::FromRawHandle};

    #[test]
    #[ignore]
    fn init_syslog() {
        init().unwrap();
    }

    #[test]
    #[ignore]
    fn syslog_log() {
        init().unwrap();
        log(
            Priority::Error,
            Facility::User,
            Some((file!(), line!())),
            format_args!("hello syslog"),
        );
    }

    #[test]
    #[ignore]
    fn proc_name() {
        init().unwrap();
        log(
            Priority::Error,
            Facility::User,
            Some((file!(), line!())),
            format_args!("before proc name"),
        );
        set_proc_name("win_sys_util-test");
        log(
            Priority::Error,
            Facility::User,
            Some((file!(), line!())),
            format_args!("after proc name"),
        );
    }

    #[test]
    #[ignore]
    fn macros() {
        init().unwrap();
        error!("this is an error {}", 3);
        warn!("this is a warning {}", "uh oh");
        info!("this is info {}", true);
        debug!("this is debug info {:?}", Some("helpful stuff"));
    }

    #[test]
    #[ignore]
    fn syslogger_char() {
        init().unwrap();
        let mut syslogger = Syslogger::new(Priority::Info, Facility::Daemon);

        let string = "Writing chars to syslog";
        for c in string.chars() {
            syslogger.write_all(&[c as u8]).expect("error writing char");
        }

        syslogger
            .write_all(&[b'\n'])
            .expect("error writing newline char");
    }

    #[test]
    #[ignore]
    fn syslogger_line() {
        init().unwrap();
        let mut syslogger = Syslogger::new(Priority::Info, Facility::Daemon);

        let s = "Writing string to syslog\n";
        syslogger
            .write_all(s.as_bytes())
            .expect("error writing string");
    }

    #[test]
    #[ignore]
    fn syslogger_partial() {
        init().unwrap();
        let mut syslogger = Syslogger::new(Priority::Info, Facility::Daemon);

        let s = "Writing partial string";
        // Should not log because there is no newline character
        syslogger
            .write_all(s.as_bytes())
            .expect("error writing string");
    }

    fn clear_path_log_levels() {
        syslog_lock!().path_log_levels.clear();
    }

    #[test]
    #[ignore]
    fn syslogger_verify_line_written() {
        init().unwrap();
        clear_path_log_levels();
        let (mut reader, writer) =
            StreamChannel::pair(BlockingMode::Blocking, FramingMode::Byte).unwrap();

        // Safe because writer is guaranteed to exist, and we forget the StreamChannel that used
        // to own the StreamChannel.
        let writer_file = unsafe { File::from_raw_handle(writer.as_raw_descriptor()) };
        std::mem::forget(writer);

        echo_file(Some(writer_file));
        error!("Test message.");

        // Ensure the message we wrote actually was written to the supplied "file" (which is
        // really a named pipe here).
        let mut buf: [u8; 1024] = [0; 1024];
        let bytes_read = reader.read(&mut buf).unwrap();
        assert!(bytes_read > 0);
        let log_msg = String::from_utf8(buf.to_vec()).unwrap();
        let re = Regex::new(r"^\[.+:ERROR:.+:[0-9]+\] Test message.").unwrap();
        assert!(re.is_match(&log_msg));
    }

    #[test]
    #[ignore]
    fn log_priority_try_from_number() {
        assert_eq!("0".try_into(), Ok(Priority::Emergency));
        assert!(Priority::try_from("100").is_err());
    }

    #[test]
    #[ignore]
    fn log_priority_try_from_words() {
        assert_eq!("EMERGENCY".try_into(), Ok(Priority::Emergency));
        assert!(Priority::try_from("_EMERGENCY").is_err());
    }

    #[test]
    #[ignore]
    fn log_priority_filter_try_from_star() {
        assert_eq!("*".try_into(), Ok(PriorityFilter::ShowAll));
    }

    #[test]
    #[ignore]
    fn log_priority_filter_try_from_silence() {
        assert_eq!("S".try_into(), Ok(PriorityFilter::Silent));
        assert_eq!("s".try_into(), Ok(PriorityFilter::Silent));
        assert_eq!("SILENT".try_into(), Ok(PriorityFilter::Silent));
    }

    #[test]
    #[ignore]
    fn log_priority_filter_try_from_priority_str() {
        assert_eq!(
            "DEBUG".try_into(),
            Ok(PriorityFilter::Priority(Priority::Debug))
        );
        assert_eq!(
            "debug".try_into(),
            Ok(PriorityFilter::Priority(Priority::Debug))
        );
        assert!(PriorityFilter::try_from("_DEBUG").is_err());
    }

    #[test]
    #[ignore]
    fn log_should_always_be_enabled_for_level_show_all() {
        init().unwrap();
        clear_path_log_levels();
        set_log_level(PriorityFilter::ShowAll);
        assert!(log_enabled(Priority::Debug, "").unwrap());
    }

    #[test]
    #[ignore]
    fn log_should_always_be_disabled_for_level_silent() {
        init().unwrap();
        clear_path_log_levels();
        set_log_level(PriorityFilter::Silent);
        let enabled = log_enabled(Priority::Emergency, "");
        set_log_level(PriorityFilter::ShowAll);
        assert!(!enabled.unwrap());
    }

    #[test]
    #[ignore]
    fn log_should_be_enabled_if_filter_level_has_a_lower_or_equal_priority() {
        init().unwrap();
        clear_path_log_levels();
        set_log_level(Priority::Info);
        let info_enabled = log_enabled(Priority::Info, "");
        let warn_enabled = log_enabled(Priority::Warning, "");
        set_log_level(PriorityFilter::ShowAll);
        assert!(info_enabled.unwrap());
        assert!(warn_enabled.unwrap());
    }

    #[test]
    #[ignore]
    fn log_should_be_disabled_if_filter_level_has_a_higher_priority() {
        init().unwrap();
        clear_path_log_levels();
        set_log_level(Priority::Info);
        let enabled = log_enabled(Priority::Debug, "");
        set_log_level(PriorityFilter::ShowAll);
        assert!(!enabled.unwrap());
    }

    #[test]
    #[ignore]
    fn path_overides_should_apply_to_logs() {
        init().unwrap();
        clear_path_log_levels();
        set_log_level(Priority::Info);
        add_path_log_level("fake/debug/src", Priority::Debug);

        assert!(!log_enabled(Priority::Debug, "test.rs").unwrap());
        assert!(log_enabled(Priority::Debug, "fake/debug/src/test.rs").unwrap());
    }

    #[test]
    #[ignore]
    fn longest_path_prefix_match_should_apply_if_multiple_filters_match() {
        init().unwrap();
        clear_path_log_levels();
        set_log_level(Priority::Info);
        add_path_log_level("fake/debug/src", Priority::Debug);
        add_path_log_level("fake/debug/src/silence", PriorityFilter::Silent);

        assert!(!log_enabled(Priority::Info, "fake/debug/src/silence/test.rs").unwrap());
    }

    #[test]
    #[ignore]
    fn syslogger_log_level_filter() {
        init().unwrap();
        clear_path_log_levels();
        set_log_level(Priority::Error);

        let (mut reader, writer) =
            StreamChannel::pair(BlockingMode::Blocking, FramingMode::Byte).unwrap();

        // Safe because writer is guaranteed to exist, and we forget the StreamChannel that used
        // to own the StreamChannel.
        let writer_file = unsafe { File::from_raw_handle(writer.as_raw_descriptor()) };
        std::mem::forget(writer);

        echo_file(Some(writer_file));
        error!("Test message.");
        debug!("Test message.");

        add_path_log_level(file!(), Priority::Debug);
        debug!("Test with file filter.");

        set_log_level(PriorityFilter::ShowAll);

        // Ensure the message we wrote actually was written to the supplied "file" (which is
        // really a named pipe here).
        let mut buf: [u8; 1024] = [0; 1024];
        let bytes_read = reader.read(&mut buf).unwrap();
        assert!(bytes_read > 0);
        let log_msg = String::from_utf8(buf.to_vec()).unwrap();
        let re_error = Regex::new(r"(?m)^\[.*ERROR:.+:[0-9]+\] Test message.").unwrap();
        let re_debug = Regex::new(r"(?m)^\[.*DEBUG:.+:[0-9]+\] Test message.").unwrap();
        let filter_debug =
            Regex::new(r"(?m)^\[.*DEBUG:.+:[0-9]+\] Test with file filter.").unwrap();
        assert!(re_error.is_match(&log_msg));
        assert!(!re_debug.is_match(&log_msg));
        assert!(filter_debug.is_match(&log_msg));
    }
}
