//! NDJSON encoding of a results stream.

use std::fmt;

use crate::Record;

/// A line of a results stream that is not a valid record.
#[derive(Debug)]
pub struct NdjsonError {
    /// 1-based line number.
    pub line: usize,
    pub source: serde_json::Error,
}

impl fmt::Display for NdjsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.source)
    }
}

impl std::error::Error for NdjsonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Encode records as NDJSON, one record per line, each line ending in `\n`.
pub fn to_ndjson<'a>(records: impl IntoIterator<Item = &'a Record>) -> String {
    let mut out = String::new();
    for record in records {
        // Serializing these plain data types cannot fail.
        out.push_str(&serde_json::to_string(record).expect("record serializes"));
        out.push('\n');
    }
    out
}

/// Decode an NDJSON stream. Blank lines are skipped; the first invalid line
/// is an error naming its line number.
pub fn parse_ndjson(input: &str) -> Result<Vec<Record>, NdjsonError> {
    let mut records = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(line).map_err(|source| NdjsonError {
            line: index + 1,
            source,
        })?;
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TargetRecord, TargetStatus, TestOutcome, TestRecord};

    fn sample() -> Vec<Record> {
        vec![
            Record::Target(TargetRecord {
                address: "//a:test".into(),
                input_hash: Some("sha256:1".into()),
                status: TargetStatus::Passed,
                cached: false,
                duration_ms: 10,
                capture: Some("junit".into()),
                confidence: None,
                exit_code: Some(0),
            }),
            Record::Test(TestRecord {
                target: "//a:test".into(),
                test_id: "x".into(),
                file: None,
                suite: None,
                name: "t".into(),
                outcome: TestOutcome::Pass,
                attempt: 1,
                duration_ms: Some(3),
                failure_hash: None,
                message: None,
            }),
        ]
    }

    #[test]
    fn round_trips_one_record_per_line() {
        let records = sample();
        let encoded = to_ndjson(&records);
        assert_eq!(encoded.lines().count(), 2);
        assert!(encoded.ends_with('\n'));
        assert_eq!(parse_ndjson(&encoded).unwrap(), records);
    }

    #[test]
    fn skips_blank_lines() {
        let encoded = format!("\n{}\n\n", to_ndjson(&sample()));
        assert_eq!(parse_ndjson(&encoded).unwrap().len(), 2);
    }

    #[test]
    fn names_the_bad_line() {
        let encoded = format!("{}{{\"type\":\"nope\"}}\n", to_ndjson(&sample()));
        let error = parse_ndjson(&encoded).unwrap_err();
        assert_eq!(error.line, 3);
        assert!(error.to_string().starts_with("line 3:"));
    }
}
