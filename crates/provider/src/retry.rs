//! Backoff and failure classification.
//!
//! Retry lives behind the [`Provider`](crate::Provider) trait, so the loop never
//! sees a 429. The classification is the plan's table:
//!
//! | Condition | Class | Action |
//! |---|---|---|
//! | 429 | `RateLimited` | Honor `retry-after` exactly if present, else backoff |
//! | 529 | `Overloaded` | Backoff |
//! | other 5xx | `Server` | Backoff |
//! | reset, TLS, DNS, timeout | `Transport` | Backoff |
//! | stream dies before `message_stop` | `StreamBroken` | Backoff, re-send whole |
//! | 400, 401, 403, 404, 413 | `Fatal` | Surface |

use std::time::Duration;

use rand::Rng;

use crate::provider::ProviderError;

// ========================================================================
// Constants
// ========================================================================

pub const MAX_ATTEMPTS: u32 = 5;
const BASE_BACKOFF: Duration = Duration::from_secs(1);
const BACKOFF_FACTOR: u32 = 2;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A ceiling on `retry-after`.
///
/// "Honor `retry-after` exactly" means do not jitter it, double it, or second
/// guess it by a few seconds. It does not mean sleep for as long as any header
/// asks: `retry-after: 999999999`, or an HTTP-date far in the future, would
/// otherwise park the agent for decades with no journal event and no way out.
/// Waking early costs one wasted attempt; not waking costs the session.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

const OVERLOADED_STATUS: u16 = 529;

// ========================================================================
// Functions
// ========================================================================

/// How long to wait before `attempt` (1-based).
///
/// `retry-after` overrides the computed schedule whenever the header is present:
/// a server telling you when to come back beats a guess.
///
/// Otherwise: exponential with **full jitter**, base 1s, factor 2, cap 60s. The
/// jitter is not decoration. Without it every agent that hit the same rate limit
/// retries in lockstep and hits it again together.
pub fn backoff(error: &ProviderError, attempt: u32) -> Duration {
    if let ProviderError::RateLimited {
        retry_after: Some(delay),
    } = error
    {
        return (*delay).min(MAX_RETRY_AFTER);
    }
    jitter(ceiling(attempt))
}

/// The exponential ceiling for `attempt`, before jitter.
fn ceiling(attempt: u32) -> Duration {
    let exponent = BACKOFF_FACTOR.saturating_pow(attempt.saturating_sub(1));
    BASE_BACKOFF.saturating_mul(exponent).min(MAX_BACKOFF)
}

/// Full jitter: uniform over `[0, ceiling]`, not `ceiling` plus a wobble.
fn jitter(ceiling: Duration) -> Duration {
    let millis = ceiling.as_millis() as u64;
    if millis == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(rand::thread_rng().gen_range(0..=millis))
}

/// Classify a non-success response. Reads the body, because that is where the
/// API names the parameter it rejected.
pub async fn classify(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = parse_retry_after(&response);
    let body = response.text().await.unwrap_or_default();

    match status {
        429 => ProviderError::RateLimited { retry_after },
        OVERLOADED_STATUS => ProviderError::Overloaded,
        400 | 401 | 403 | 404 | 413 => ProviderError::Fatal {
            kind: error_type(&body),
            message: body,
            status: Some(status),
        },
        500..=599 => ProviderError::Server { status },
        _ => ProviderError::Fatal {
            kind: error_type(&body),
            message: body,
            status: Some(status),
        },
    }
}

/// Map a `reqwest` send failure onto the transport class. These retry: the plan
/// is explicit that a connection reset is not a request the server rejected.
pub fn classify_transport(error: &reqwest::Error) -> ProviderError {
    ProviderError::Transport(error.to_string())
}

/// `retry-after` in either form the HTTP spec allows: delta-seconds, or an
/// HTTP-date. Parsing only the integer form silently drops the date form onto
/// the backoff path.
fn parse_retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    parse_http_date_delay(value.trim())
}

/// An HTTP-date `retry-after`, as a delay from now. `None` if it is in the past
/// or unparseable.
fn parse_http_date_delay(value: &str) -> Option<Duration> {
    let target = httpdate_to_unix(value)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    target.checked_sub(now).map(Duration::from_secs)
}

/// Parse an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) to a unix timestamp.
/// This is the only date form worth supporting: RFC 9110 requires senders to use
/// it.
fn httpdate_to_unix(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    parts.next()?; // Weekday, which the timestamp does not need.
    let day: u64 = parts.next()?.parse().ok()?;
    let month = month_number(parts.next()?)?;
    let year: u64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;

    let days = days_from_civil(year, month, day)?;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn month_number(name: &str) -> Option<u64> {
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    months
        .iter()
        .position(|month| *month == name)
        .map(|index| index as u64 + 1)
}

/// Days since the unix epoch, by Howard Hinnant's civil-date algorithm.
///
/// Every subtraction is checked: this parses an attacker-reachable header, and a
/// year of `0000` would otherwise underflow the `month <= 2` adjustment and
/// panic the provider on a malformed `retry-after`.
fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    if !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    let year = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * 146_097 + day_of_era).checked_sub(719_468)
}

/// The `error.type` field of an API error body, if it parses.
fn error_type(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/type")
                .and_then(|kind| kind.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

// ========================================================================
// Tests
// ========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_overrides_the_schedule_exactly() {
        let error = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(17)),
        };
        // Not jittered, not capped, not doubled: exactly what the server said.
        for attempt in 1..=MAX_ATTEMPTS {
            assert_eq!(backoff(&error, attempt), Duration::from_secs(17));
        }
    }

    #[test]
    fn an_absurd_retry_after_is_capped() {
        // Honoring it "exactly" must not mean parking the agent for 30 years on
        // a header nobody can see.
        let error = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(999_999_999)),
        };
        assert_eq!(backoff(&error, 1), MAX_RETRY_AFTER);
    }

    #[test]
    fn backoff_is_exponential_base_1s_factor_2_capped_at_60s() {
        assert_eq!(ceiling(1), Duration::from_secs(1));
        assert_eq!(ceiling(2), Duration::from_secs(2));
        assert_eq!(ceiling(3), Duration::from_secs(4));
        assert_eq!(ceiling(4), Duration::from_secs(8));
        assert_eq!(ceiling(5), Duration::from_secs(16));
        assert_eq!(ceiling(30), MAX_BACKOFF);
    }

    #[test]
    fn jitter_is_full_and_bounded_by_the_ceiling() {
        // Full jitter means uniform over [0, ceiling]. The bug this catches is
        // "ceiling plus a wobble", which does not decorrelate a retrying fleet.
        let error = ProviderError::Overloaded;
        let mut seen_below_half = false;
        for _ in 0..200 {
            let delay = backoff(&error, 5);
            assert!(
                delay <= ceiling(5),
                "jitter exceeded the ceiling: {delay:?}"
            );
            if delay < ceiling(5) / 2 {
                seen_below_half = true;
            }
        }
        assert!(
            seen_below_half,
            "no delay landed in the lower half: jitter is not full"
        );
    }

    #[test]
    fn only_fatal_is_terminal() {
        assert!(
            !ProviderError::Fatal {
                kind: "invalid_request_error".into(),
                message: String::new(),
                status: Some(400),
            }
            .is_retryable()
        );

        assert!(ProviderError::RateLimited { retry_after: None }.is_retryable());
        assert!(ProviderError::Overloaded.is_retryable());
        assert!(ProviderError::Server { status: 503 }.is_retryable());
        assert!(ProviderError::Transport("reset".into()).is_retryable());
        assert!(ProviderError::StreamBroken("cut".into()).is_retryable());
    }

    #[test]
    fn error_type_is_read_out_of_the_api_body() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
        assert_eq!(error_type(body), "invalid_request_error");
        assert_eq!(error_type("not json"), "unknown");
    }

    #[test]
    fn http_date_retry_after_parses() {
        // The integer form is the common one; the date form is the one a
        // seconds-only parser silently drops onto the backoff path.
        assert_eq!(
            httpdate_to_unix("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(httpdate_to_unix("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(httpdate_to_unix("garbage"), None);
    }

    #[test]
    fn a_malformed_retry_after_date_does_not_panic() {
        // The header is attacker-reachable, so every parse path must fail
        // rather than fault. Year 0000 with a January date is the one that
        // underflows the civil-date adjustment.
        assert_eq!(httpdate_to_unix("Sat, 01 Jan 0000 00:00:00 GMT"), None);
        assert_eq!(httpdate_to_unix("Sat, 00 Jan 2026 00:00:00 GMT"), None);
        assert_eq!(httpdate_to_unix("Sat, 01 Xxx 2026 00:00:00 GMT"), None);
        assert_eq!(httpdate_to_unix(""), None);
        assert_eq!(httpdate_to_unix("Sat, 01 Jan 2026"), None);
    }
}
