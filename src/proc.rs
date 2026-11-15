//! The few pure parsers the memory facts need.
//!
//! `cdct` already has these parsers, but it is the conductor: making a driver
//! depend on it inverts the layering, and it is edited on its own schedule.
//! The right home is `cdrv`, beside [`MemoryFacts`](cdrv::MemoryFacts); until
//! that move happens this crate carries the minimal subset it needs. The
//! README lists the contract change.

use thiserror::Error;

/// A parse failure with the input that caused it.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ParseError {
    /// A required key was absent.
    #[error("{file} has no {key}")]
    Missing {
        /// The file the text came from.
        file: &'static str,
        /// The key that was required.
        key: &'static str,
    },
    /// A value was present but not a number.
    #[error("{file}: {key} has a non-numeric value {value:?}")]
    NotANumber {
        /// The file the text came from.
        file: &'static str,
        /// The key whose value failed.
        key: String,
        /// The offending value.
        value: String,
    },
}

/// `MemAvailable` from `/proc/meminfo`, in MiB.
pub(crate) fn parse_mem_available_mib(text: &str) -> Result<u64, ParseError> {
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != "MemAvailable" {
            continue;
        }
        let raw = rest.split_whitespace().next().ok_or(ParseError::Missing {
            file: "meminfo",
            key: "MemAvailable",
        })?;
        let kib: u64 = raw.parse().map_err(|_| ParseError::NotANumber {
            file: "meminfo",
            key: "MemAvailable".to_owned(),
            value: raw.to_owned(),
        })?;
        return Ok(kib / 1024);
    }
    Err(ParseError::Missing {
        file: "meminfo",
        key: "MemAvailable",
    })
}

/// The reclaimable part of the ZFS ARC from `arcstats`, in MiB.
///
/// That is the four evictable counters, capped at `size - c_min`: below
/// `c_min` the shrinker keeps the ARC, so none of it is headroom. A counter
/// that is absent counts as zero (the safe direction).
pub(crate) fn parse_evictable_arc_mib(text: &str) -> Result<u64, ParseError> {
    let mut size = 0_u64;
    let mut c_min = 0_u64;
    let mut evictable = 0_u64;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(_kind), Some(raw)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // Unknown rows (the kstat header included) are skipped before their
        // value is parsed: the header's value is a hex marker, not a counter.
        if !matches!(
            name,
            "size"
                | "c_min"
                | "mru_evictable_data"
                | "mru_evictable_metadata"
                | "mfu_evictable_data"
                | "mfu_evictable_metadata"
        ) {
            continue;
        }
        let value: u64 = raw.parse().map_err(|_| ParseError::NotANumber {
            file: "arcstats",
            key: name.to_owned(),
            value: raw.to_owned(),
        })?;
        match name {
            "size" => size = value,
            "c_min" => c_min = value,
            _ => evictable = evictable.saturating_add(value),
        }
    }
    let cap = size.saturating_sub(c_min);
    Ok(evictable.min(cap) / 1_048_576)
}

/// The `full avg10` share of `/proc/pressure/memory`, when reported.
pub(crate) fn parse_pressure_full_avg10(text: &str) -> Result<Option<f32>, ParseError> {
    let mut saw_line = false;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(kind) = fields.next() else {
            continue;
        };
        if kind != "some" && kind != "full" {
            continue;
        }
        saw_line = true;
        if kind != "full" {
            continue;
        }
        for field in fields {
            if let Some(value) = field.strip_prefix("avg10=") {
                return value.parse().map(Some).map_err(|_| ParseError::NotANumber {
                    file: "pressure/memory",
                    key: "full avg10".to_owned(),
                    value: value.to_owned(),
                });
            }
        }
        return Ok(None);
    }
    if saw_line {
        Ok(None)
    } else {
        Err(ParseError::Missing {
            file: "pressure/memory",
            key: "some or full",
        })
    }
}
