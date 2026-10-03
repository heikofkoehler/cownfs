//! Structured JSON logging.

use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(1); // 0=error, 1=warn, 2=info, 3=debug

pub fn set_level(level: &str) {
    let v = match level {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        "debug" => 3,
        _ => 1,
    };
    LEVEL.store(v, Ordering::Relaxed);
}

fn log(level: &str, lvl_num: u8, msg: &str, fields: &[(&str, &str)]) {
    if LEVEL.load(Ordering::Relaxed) < lvl_num {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut out = format!("{{\"ts\":{ts},\"level\":\"{level}\",\"msg\":\"{msg}\"");
    for (k, v) in fields {
        out.push_str(&format!(",\"{k}\":\"{v}\""));
    }
    out.push('}');
    eprintln!("{out}");
}

pub fn error(msg: &str, fields: &[(&str, &str)]) {
    log("error", 0, msg, fields);
}
pub fn warn(msg: &str, fields: &[(&str, &str)]) {
    log("warn", 1, msg, fields);
}
pub fn info(msg: &str, fields: &[(&str, &str)]) {
    log("info", 2, msg, fields);
}
pub fn debug(msg: &str, fields: &[(&str, &str)]) {
    log("debug", 3, msg, fields);
}

/// Audit log: mutating operations with client identity.
/// Always logged at info level, separate from debug logs.
pub fn audit(op: &str, client: &str, uid: u32, details: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    eprintln!(
        "{{\"ts\":{ts},\"level\":\"audit\",\"op\":\"{op}\",\"client\":\"{client}\",\"uid\":{uid},\"details\":\"{details}\"}}"
    );
}
