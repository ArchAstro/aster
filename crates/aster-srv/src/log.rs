//! One JSON object per log line, in the shape Cloud Logging reads
//! (`severity`, `message`).

use serde_json::{json, Map, Value};

fn emit(severity: &str, message: &str, fields: Value) {
    let mut line = Map::new();
    line.insert("severity".into(), json!(severity));
    line.insert("message".into(), json!(message));
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    println!("{}", Value::Object(line));
}

pub fn info(message: &str, fields: Value) {
    emit("INFO", message, fields);
}

pub fn warn(message: &str, fields: Value) {
    emit("WARNING", message, fields);
}

pub fn error(message: &str, fields: Value) {
    emit("ERROR", message, fields);
}
