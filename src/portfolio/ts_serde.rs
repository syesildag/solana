//! Shared serde helpers for persisting epoch-second timestamps as
//! human-readable RFC3339 strings in JSON.
//!
//! The in-memory type stays `i64` (epoch seconds) so all timestamp arithmetic —
//! hold time, daily caps, cooldowns — is untouched; only the on-disk
//! representation changes. The deserializer accepts BOTH an RFC3339 string and a
//! bare integer, so state files written before the datetime switch still load
//! and migrate in place on the next save.
//!
//! Usage:
//! - scalar field:  `#[serde(with = "crate::portfolio::ts_serde::rfc3339")]`
//! - map of values: `#[serde(with = "crate::portfolio::ts_serde::rfc3339_map")]`
//!   (on a `HashMap<String, i64>`)

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde::Deserialize;

/// An epoch-second timestamp on the wire: either a bare integer (the legacy
/// format) or an RFC3339 string.
#[derive(Deserialize)]
#[serde(untagged)]
enum TsRepr {
    Int(i64),
    Str(String),
}

/// Epoch seconds → RFC3339 UTC string (whole seconds, `Z` suffix), e.g.
/// `2026-06-06T12:08:24Z`.
fn epoch_to_rfc3339(ts: i64) -> Result<String, String> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .ok_or_else(|| format!("out-of-range timestamp {ts}"))
}

/// RFC3339 string (or legacy integer) → epoch seconds.
fn repr_to_epoch(repr: TsRepr) -> Result<i64, String> {
    match repr {
        TsRepr::Int(n) => Ok(n),
        TsRepr::Str(s) => DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.timestamp())
            .map_err(|e| format!("invalid RFC3339 timestamp {s:?}: {e}")),
    }
}

/// `#[serde(with = "...::rfc3339")]` for a scalar `i64` epoch-second field.
pub mod rfc3339 {
    use super::{epoch_to_rfc3339, repr_to_epoch, TsRepr};
    use serde::{de::Error as _, ser::Error as _, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(ts: &i64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&epoch_to_rfc3339(*ts).map_err(S::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        repr_to_epoch(TsRepr::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// `#[serde(with = "...::rfc3339_map")]` for a `HashMap<String, i64>` whose
/// values are epoch-second timestamps.
pub mod rfc3339_map {
    use std::collections::HashMap;

    use super::{epoch_to_rfc3339, repr_to_epoch, TsRepr};
    use serde::{
        de::Error as _, ser::Error as _, ser::SerializeMap, Deserialize, Deserializer, Serializer,
    };

    pub fn serialize<S: Serializer>(m: &HashMap<String, i64>, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(m.len()))?;
        for (k, v) in m {
            map.serialize_entry(k, &epoch_to_rfc3339(*v).map_err(S::Error::custom)?)?;
        }
        map.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<HashMap<String, i64>, D::Error> {
        HashMap::<String, TsRepr>::deserialize(d)?
            .into_iter()
            .map(|(k, v)| repr_to_epoch(v).map(|ts| (k, ts)).map_err(D::Error::custom))
            .collect()
    }
}
