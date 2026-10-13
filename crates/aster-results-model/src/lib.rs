//! Test result records shared by the Aster CLI and the `aster-results`
//! service.
//!
//! One Aster invocation produces one NDJSON stream: a [`RunRecord`] first,
//! then a [`TargetRecord`] for every target it ran and a [`TestRecord`] for
//! every test attempt inside those targets. The CLI writes the stream with
//! [`to_ndjson`] and the service reads it back with [`parse_ndjson`].

mod failure;
mod identity;
mod ndjson;
mod record;

pub use failure::{failure_hash, scrub_message, MAX_MESSAGE_BYTES};
pub use identity::TestIdentity;
pub use ndjson::{parse_ndjson, to_ndjson, NdjsonError};
pub use record::{
    Confidence, Record, RunRecord, TargetRecord, TargetStatus, TestOutcome, TestRecord,
};

pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
