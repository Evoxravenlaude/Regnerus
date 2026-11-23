//! Log lines in the shape the Kizzy watcher uses:
//! `2026-11-22 08:19:01.123 INFO pipeline: stream connected`, in UTC.

use std::io::Write;

use log::{Level, LevelFilter, Metadata, Record};

struct Logger {
    level: LevelFilter,
}

const CRATE: &str = "regnerus";

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        if metadata.target().starts_with(CRATE) {
            metadata.level() <= self.level
        } else {
            // Libraries only get a say when something is wrong.
            metadata.level() <= self.level.min(LevelFilter::Warn)
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let at = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARNING",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let target = record.target();
        let name = target.rsplit("::").next().unwrap_or(target);
        let name = if name == CRATE { "main" } else { name };
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{at} {level} {name}: {}", record.args());
    }

    fn flush(&self) {
        let _ = std::io::stdout().flush();
    }
}

pub fn level_from(name: &str) -> LevelFilter {
    match name.trim().to_uppercase().as_str() {
        "TRACE" => LevelFilter::Trace,
        "DEBUG" => LevelFilter::Debug,
        "WARNING" | "WARN" => LevelFilter::Warn,
        "ERROR" | "CRITICAL" | "FATAL" => LevelFilter::Error,
        "OFF" => LevelFilter::Off,
        _ => LevelFilter::Info,
    }
}

/// Installs the logger once; later calls are ignored.
pub fn init(level: &str) {
    let level = level_from(level);
    if log::set_boxed_logger(Box::new(Logger { level })).is_ok() {
        log::set_max_level(level);
    }
}
