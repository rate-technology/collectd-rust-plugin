use bindings::{plugin_log, LOG_DEBUG, LOG_ERR, LOG_INFO, LOG_NOTICE, LOG_WARNING};
use env_logger::filter;
use log::{self, Level, LevelFilter, Metadata, Record, SetLoggerError};
use plugins::PluginManager;
use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::io::{self, Cursor, Write};
use std::mem;

/// Bridges the gap between collectd and rust logging. Terminology and filters methods found here
/// are from env_logger.
///
/// It is recommended to instantiate the logger in `PluginManager::plugins`.
///
/// The use case of multiple rust plugins that instantiate a global logger is supported. Each
/// plugin will have its own copy of a global logger. Thus plugins won't interefere with others and
/// their logging.
///
/// # Example
///
/// ```
/// # extern crate collectd_plugin;
/// # extern crate failure;
/// # extern crate log;
/// # fn main() {
/// use collectd_plugin::{ConfigItem, PluginManager, PluginRegistration, CollectdLoggerBuilder};
/// use failure::Error;
/// use log::LevelFilter;
///
/// #[derive(Default)]
/// struct MyPlugin;
/// impl PluginManager for MyPlugin {
///     fn name() -> &'static str {
///         "myplugin"
///     }
///
///     fn plugins(_config: Option<&[ConfigItem]>) -> Result<PluginRegistration, Error> {
///        CollectdLoggerBuilder::new()
///            .prefix_plugin::<Self>()
///            .filter_level(LevelFilter::Info)
///            .try_init()
///            .expect("really the only thing that should create a logger");
///         unimplemented!()
///     }
/// }
/// # }
/// ```
#[derive(Default)]
pub struct CollectdLoggerBuilder {
    filter: filter::Builder,
    plugin: Option<&'static str>,
    format: Format,
}

type FormatFn = Fn(&mut Write, &Record) -> io::Result<()> + Sync + Send;

impl CollectdLoggerBuilder {
    /// Initializes the log builder with defaults
    pub fn new() -> Self {
        Default::default()
    }

    /// Initializes the global logger with the built collectd logger.
    ///
    /// # Errors
    ///
    /// This function will fail if it is called more than once, or if another
    /// library has already initialized a global logger.
    pub fn try_init(&mut self) -> Result<(), SetLoggerError> {
        let logger = CollectdLogger {
            filter: self.filter.build(),
            plugin: self.plugin,
            format: mem::replace(&mut self.format, Default::default()).into_boxed_fn(),
        };

        log::set_max_level(logger.filter());
        log::set_boxed_logger(Box::new(logger))
    }

    /// Prefixes all log messages with a plugin's name. This is recommended to aid debugging and
    /// gain insight into collectd logs.
    pub fn prefix_plugin<T: PluginManager>(&mut self) -> &mut Self {
        self.plugin = Some(T::name());
        self
    }

    /// See
    /// [`env_logger::Builder::filter_level`](https://docs.rs/env_logger/0.5.13/env_logger/struct.Builder.html#method.filter_level)
    pub fn filter_level(&mut self, level: LevelFilter) -> &mut Self {
        self.filter.filter_level(level);
        self
    }

    /// See:
    /// [`env_logger::Builder::filter_module`](https://docs.rs/env_logger/0.5.13/env_logger/struct.Builder.html#method.filter_module)
    pub fn filter_module(&mut self, module: &str, level: LevelFilter) -> &mut Self {
        self.filter.filter_module(module, level);
        self
    }

    /// See:
    /// [`env_logger::Builder::filter`](https://docs.rs/env_logger/0.5.13/env_logger/struct.Builder.html#method.filter)
    pub fn filter(&mut self, module: Option<&str>, level: LevelFilter) -> &mut Self {
        self.filter.filter(module, level);
        self
    }

    /// See: [`env_logger::Builder::parse`](https://docs.rs/env_logger/0.5.13/env_logger/struct.Builder.html#method.parse)
    pub fn parse(&mut self, filters: &str) -> &mut Self {
        self.filter.parse(filters);
        self
    }

    /// Sets the format function for formatting the log output.
    pub fn format<F: 'static>(&mut self, format: F) -> &mut Self
        where F: Fn(&mut Write, &Record) -> io::Result<()> + Sync + Send
    {
        self.format.custom_format = Some(Box::new(format));
        self
	}
}

#[derive(Default)]
struct Format {
    custom_format: Option<Box<FormatFn>>,
}

impl Format {
    fn into_boxed_fn(self) -> Box<FormatFn> {
        if let Some(fmt) = self.custom_format {
            fmt
        } else {
            Box::new(move |buf, record| {
                if let Some(path) = record.module_path() {
                    write!(buf, "{}: ", path)?;
                }

                write!(buf, "{}", record.args())
            })
        }
    }
}

struct CollectdLogger {
    filter: filter::Filter,
    plugin: Option<&'static str>,
    format: Box<FormatFn>,
}

impl log::Log for CollectdLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.filter.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if self.matches(record) {
            // Log records are written to a thread local storage before being submitted to
            // collectd. The buffers are cleared afterwards
            thread_local!(static LOG_BUF: Cell<Vec<u8>> = Cell::new(Vec::new()));
            LOG_BUF.with(|cell| {
                // Replaces the cell's contents with the default value, which is an empty vector.
                // Should be very cheap to move in and out of
                let v = cell.take();
                let mut curse = Cursor::new(v);
                if let Some(plugin) = self.plugin {
                    write!(curse, "{}: ", plugin);
                }

                let mut new_vec = if (self.format)(&mut curse, record).is_ok() {
                    let lvl = LogLevel::from(record.level());
                    let mut nv = curse.into_inner();

                    // Force a trailing NUL so that we can use fast path
                    nv.push(b'\0');
                    {
                        let cs = unsafe { CStr::from_bytes_with_nul_unchecked(&nv[..]) };
                        unsafe { plugin_log(lvl as i32, cs.as_ptr()) };
                    }

                    nv
                } else {
                    curse.into_inner()
                };

                new_vec.clear();
                cell.set(new_vec);
            });
        }
    }

    fn flush(&self) {}
}

impl CollectdLogger {
    /// Checks if this record matches the configured filter.
    pub fn matches(&self, record: &Record) -> bool {
        self.filter.matches(record)
    }

    /// Returns the maximum `LevelFilter` that this env logger instance is
    /// configured to output.
    pub fn filter(&self) -> LevelFilter {
        self.filter.filter()
    }
}

/// Sends message and log level to collectd. This bypasses any configuration setup via
/// [`CollectdLoggerBuilder`], so collectd configuration soley determines if a level is logged
/// and where it is delivered. Messages that are too long are truncated (1024 was the max length as
/// of collectd-5.7).
///
/// In general, prefer [`CollectdLoggerBuilder`] and direct all logging as one normally would
/// through the `log` crate.
///
/// # Panics
///
/// If a message containing a null character is given as a message this function will panic.
///
/// [`CollectdLoggerBuilder`]: struct.CollectdLoggerBuilder.html
pub fn collectd_log(lvl: LogLevel, message: &str) {
    let cs = CString::new(message).expect("Collectd log to not contain nulls");
    unsafe {
        // Collectd will allocate another string behind the scenes before passing to plugins that
        // registered a log hook, so passing it a string slice is fine.
        plugin_log(lvl as i32, cs.as_ptr());
    }
}

/// A simple wrapper around the collectd's plugin_log, which in turn wraps `vsnprintf`.
///
/// ```ignore
/// collectd_log_raw!(LogLevel::Info, b"test %d\0", 10);
/// ```
///
/// Since this is a low level wrapper, prefer `CollectdLoggerBuilder` or even `collectd_log`. The only times you would
/// prefer `collectd_log_raw`:
///
/// - Collectd was not compiled with `COLLECTD_DEBUG` (chances are, your collectd is compiled with
/// debugging enabled) and you are logging a debug message.
/// - The performance price of string formatting in rust instead of C is too large (this shouldn't
/// be the case)
///
/// Undefined behavior can result from any of the following:
///
/// - If the format string is not null terminated
/// - If any string arguments are not null terminated. Use `CString::as_ptr()` to ensure null
/// termination
/// - Malformed format string or mismatched arguments
///
/// This expects an already null terminated byte string. In the future once the byte equivalent of
/// `concat!` is rfc accepted and implemented, this won't be a requirement. I also wish that we
/// could statically assert that the format string contained a null byte for the last character,
/// but alas, I think we've hit brick wall with the current Rust compiler.
///
/// For ergonomic reasons, the format string is converted to the appropriate C ffi type from a byte
/// string, but no other string arguments are afforded this luxury (so make sure you convert them).
#[macro_export]
macro_rules! collectd_log_raw {
    ($lvl:expr, $fmt:expr) => ({
        let level: $crate::LogLevel = $lvl;
        let level = level as i32;
        unsafe {
            $crate::bindings::plugin_log(level, ($fmt).as_ptr() as *const i8);
        }
    });
    ($lvl:expr, $fmt:expr, $($arg:expr),*) => ({
        let level: $crate::LogLevel = $lvl;
        let level = level as i32;
        unsafe {
            $crate::bindings::plugin_log(level, ($fmt).as_ptr() as *const i8, $($arg)*);
        }
    });
}

/// The available levels that collectd exposes to log messages.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[repr(u32)]
pub enum LogLevel {
    Error = LOG_ERR,
    Warning = LOG_WARNING,
    Notice = LOG_NOTICE,
    Info = LOG_INFO,
    Debug = LOG_DEBUG,
}

impl LogLevel {
    /// Attempts to convert a u32 representing a collectd logging level into a Rust enum
    pub fn try_from(s: u32) -> Option<LogLevel> {
        match s {
            LOG_ERR => Some(LogLevel::Error),
            LOG_WARNING => Some(LogLevel::Warning),
            LOG_NOTICE => Some(LogLevel::Notice),
            LOG_INFO => Some(LogLevel::Info),
            LOG_DEBUG => Some(LogLevel::Debug),
            _ => None,
        }
    }
}

impl From<Level> for LogLevel {
    fn from(lvl: Level) -> Self {
        match lvl {
            Level::Error => LogLevel::Error,
            Level::Warn => LogLevel::Warning,
            Level::Info => LogLevel::Info,
            Level::Debug | Level::Trace => LogLevel::Debug,
        }
    }
}