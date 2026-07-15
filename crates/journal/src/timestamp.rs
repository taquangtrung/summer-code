//! `SystemTime` as RFC 3339 with milliseconds, in UTC.
//!
//! Serde's default for `SystemTime` is a `{secs_since_epoch, nanos_since_epoch}`
//! struct. That is unreadable and pins the wire format to a Rust
//! representation, so [`Record::at`](crate::Record::at) carries
//! `#[serde(with = "crate::timestamp")]` and this module says the shape once.
//!
//! The precision is fixed at three digits rather than left to RFC 3339's
//! variable subsecond width: a journal line is compared byte-for-byte by the
//! `journal-sample.jsonl` round-trip test, and a timestamp that renders
//! `.12Z` on one record and `.120Z` on another cannot be compared that way.
//! Reading accepts any RFC 3339 subsecond width; only writing is pinned.

use std::time::SystemTime;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

// ========================================================================
// Constants
// ========================================================================

/// RFC 3339, milliseconds, literal `Z`. Not `Rfc3339` itself: that well-known
/// format emits whatever subsecond width the value happens to need.
const TIMESTAMP_FORMAT: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

// ========================================================================
// Functions
// ========================================================================

pub fn serialize<S>(at: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let rendered = OffsetDateTime::from(*at)
        .format(TIMESTAMP_FORMAT)
        .map_err(serde::ser::Error::custom)?;
    serializer.serialize_str(&rendered)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
where
    D: Deserializer<'de>,
{
    let rendered = String::deserialize(deserializer)?;
    let parsed = OffsetDateTime::parse(&rendered, &Rfc3339).map_err(D::Error::custom)?;
    Ok(SystemTime::from(parsed))
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Probe {
        #[serde(with = "crate::timestamp")]
        at: SystemTime,
    }

    fn probe(at: SystemTime) -> String {
        serde_json::to_string(&Probe { at }).expect("serialize")
    }

    #[test]
    fn renders_rfc3339_millis_in_utc() {
        let at = SystemTime::UNIX_EPOCH + Duration::new(1_784_000_000, 120_000_000);
        assert_eq!(probe(at), r#"{"at":"2026-07-14T03:33:20.120Z"}"#);
    }

    #[test]
    fn pins_subsecond_width_at_three_digits() {
        // A whole second must still render `.000Z`, and sub-millisecond
        // precision must truncate rather than widen the field. Either one
        // varying would break the byte-exact sample round-trip.
        let whole = SystemTime::UNIX_EPOCH + Duration::new(1_784_000_000, 0);
        assert_eq!(probe(whole), r#"{"at":"2026-07-14T03:33:20.000Z"}"#);

        let sub_milli = SystemTime::UNIX_EPOCH + Duration::new(1_784_000_000, 120_999_999);
        assert_eq!(probe(sub_milli), r#"{"at":"2026-07-14T03:33:20.120Z"}"#);
    }

    #[test]
    fn round_trips_through_the_rendered_form() {
        let at = SystemTime::UNIX_EPOCH + Duration::new(1_784_000_000, 120_000_000);
        let encoded = probe(at);
        let decoded: Probe = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded.at, at);
    }
}
