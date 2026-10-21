use jiff::{civil::DateTime, tz::TimeZone};

/// A point in time. Gerrit sends UTC wall-clock strings; see
/// [`parse_gerrit_timestamp`].
pub type Timestamp = jiff::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid Gerrit timestamp {input:?}: {reason}")]
pub struct TimestampParseError {
    pub input: String,
    pub reason: String,
}

/// Parses Gerrit's timestamp format, e.g. `2013-02-01 09:59:32.126000000`.
///
/// The value is UTC. The fractional part may have 0 to 9 digits or be absent.
pub fn parse_gerrit_timestamp(input: &str) -> Result<Timestamp, TimestampParseError> {
    let err = |reason: String| TimestampParseError {
        input: input.to_owned(),
        reason,
    };
    let civil: DateTime = input.trim().parse().map_err(|e| err(format!("{e}")))?;
    TimeZone::UTC
        .to_timestamp(civil)
        .map_err(|e| err(format!("{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nine_digit_fraction() {
        let ts = parse_gerrit_timestamp("2013-02-01 09:59:32.126000000").unwrap();
        assert_eq!(ts.to_string(), "2013-02-01T09:59:32.126Z");
    }

    #[test]
    fn parses_other_fraction_lengths() {
        for (s, want) in [
            ("2013-02-01 09:59:32", "2013-02-01T09:59:32Z"),
            ("2013-02-01 09:59:32.1", "2013-02-01T09:59:32.1Z"),
            ("2013-02-01 09:59:32.123456", "2013-02-01T09:59:32.123456Z"),
            (
                "2013-02-01 09:59:32.123456789",
                "2013-02-01T09:59:32.123456789Z",
            ),
        ] {
            assert_eq!(parse_gerrit_timestamp(s).unwrap().to_string(), want, "{s}");
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_gerrit_timestamp("yesterday").is_err());
        assert!(parse_gerrit_timestamp("").is_err());
    }
}
