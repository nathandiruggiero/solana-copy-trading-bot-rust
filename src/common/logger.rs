use chrono::{Local, Utc};
use env_logger::Builder;
use log::Level;
use std::io::Write;

const LOG_LEVEL: &str = "LOG";

pub struct Logger {
    prefix: String,
    date_format: String,
}

impl Logger {
    pub fn new(prefix: String) -> Self {
        Logger {
            prefix,
            date_format: String::from("%Y-%m-%d %H:%M:%S"),
        }
    }

    pub fn log(&self, message: String) -> String {
        let log = format!("{} {}", self.prefix_with_date(), message);
        println!("{}", log);
        log
    }

    pub fn debug(&self, message: String) -> String {
        let log = format!("{} [{}] {}", self.prefix_with_date(), "DEBUG", message);
        if LogLevel::new().is_debug() {
            println!("{}", log);
        }
        log
    }
    pub fn error(&self, message: String) -> String {
        let log = format!("{} [{}] {}", self.prefix_with_date(), "ERROR", message);
        println!("{}", log);
        log
    }

    fn prefix_with_date(&self) -> String {
        let date = Local::now();
        format!("[{}] {}", date.format(self.date_format.as_str()), self.prefix)
    }
}

struct LogLevel<'a> {
    level: &'a str,
}
impl LogLevel<'_> {
    fn new() -> Self {
        let level = LOG_LEVEL;
        LogLevel { level }
    }
    fn is_debug(&self) -> bool {
        self.level.to_lowercase().eq("debug")
    }
}

pub fn init_logger(format: &str) {
    let mut builder = Builder::from_default_env();
    match format {
        "json" => {
            builder.format(|buf, record| {
                let ts = Utc::now().to_rfc3339();
                let level = record.level().to_string();
                let target = record.target();
                let msg = format!("{}", record.args());
                writeln!(buf, "{{\"ts\":\"{}\",\"level\":\"{}\",\"target\":\"{}\",\"msg\":\"{}\"}}", ts, level, target, escape_json(&msg))
            });
        }
        _ => {
            builder.format(|buf, record| {
                let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
                writeln!(buf, "[{}] {:>5} - {}", ts, record.level(), record.args())
            });
        }
    }
    builder.init();
}

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
