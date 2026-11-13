//! `tracing` to logcat through liblog, which every Android process has loaded. The core logs a few
//! lines per dictation and opens no spans, so this beats pulling in `tracing-subscriber` and a
//! logcat crate. Only the call into liblog is Android's; the rest builds and is linted on the host.

use std::{
    ffi::{CString, c_int},
    fmt::{self, Write as _},
};

use tracing::{
    Event, Level, Metadata, Subscriber,
    field::{Field, Visit},
    level_filters::LevelFilter,
    span,
};

/// Once per process; later calls do nothing. Panics are logged too: their default output, stderr,
/// goes nowhere on Android.
pub(crate) fn init() {
    if tracing::subscriber::set_global_default(Logcat).is_ok() {
        std::panic::set_hook(Box::new(|info| tracing::error!("{info}")));
    }
}

struct Logcat;

impl Subscriber for Logcat {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        let max = if metadata.target().starts_with("psst") { Level::DEBUG } else { Level::INFO };
        *metadata.level() <= max
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::DEBUG)
    }

    fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}

    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = Line(String::new());
        event.record(&mut line);
        // Android's priorities: DEBUG 3 to ERROR 6.
        let priority: c_int = match *event.metadata().level() {
            Level::ERROR => 6,
            Level::WARN => 5,
            Level::INFO => 4,
            _ => 3,
        };
        let text = CString::new(line.0.replace('\0', " ")).unwrap_or_default();
        platform::write(priority, &text);
    }

    fn enter(&self, _: &span::Id) {}

    fn exit(&self, _: &span::Id) {}
}

struct Line(String);

impl Line {
    fn field(&mut self, field: &Field, value: fmt::Arguments<'_>) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        if field.name() != "message" {
            let _ = write!(self.0, "{}=", field.name());
        }
        let _ = self.0.write_fmt(value);
    }
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.field(field, format_args!("{value}"));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.field(field, format_args!("{value:?}"));
    }
}

#[cfg(target_os = "android")]
mod platform {
    use std::ffi::{CStr, c_char, c_int};

    #[link(name = "log")]
    unsafe extern "C" {
        fn __android_log_write(priority: c_int, tag: *const c_char, text: *const c_char) -> c_int;
    }

    pub(super) fn write(priority: c_int, text: &CStr) {
        // SAFETY: both are NUL-terminated and outlive the call, which copies them.
        unsafe { __android_log_write(priority, c"psst".as_ptr(), text.as_ptr()) };
    }
}

#[cfg(not(target_os = "android"))]
mod platform {
    use std::ffi::{CStr, c_int};

    /// Nothing calls `init` on the host.
    pub(super) fn write(_priority: c_int, text: &CStr) {
        eprintln!("{}", text.to_string_lossy());
    }
}
