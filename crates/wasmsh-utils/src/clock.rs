//! Wall-clock and monotonic clock capabilities shared by shell utilities.

use std::cell::Cell;
use std::rc::Rc;

/// Errors returned by a host clock or by UTC date conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClockError {
    /// The host did not install a clock capability.
    Unavailable(String),
    /// The host callback failed or returned a value of the wrong type.
    Callback(String),
    /// A Unix millisecond value cannot be represented by the date formatter.
    InvalidUnixMillis(i64),
    /// A legacy date string could not be parsed or is not a valid UTC date.
    InvalidDate(String),
}

impl std::fmt::Display for ClockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "clock unavailable: {message}"),
            Self::Callback(message) => write!(f, "clock callback failed: {message}"),
            Self::InvalidUnixMillis(value) => {
                write!(f, "clock returned unsupported Unix milliseconds: {value}")
            }
            Self::InvalidDate(value) => write!(f, "invalid UTC date: {value}"),
        }
    }
}

/// A host-provided clock capability.
///
/// `now_unix_ms` is a wall clock used by `date` and AWS `SigV4`. The monotonic
/// value is independent of wall-clock adjustments and is used for shell
/// elapsed time and deadlines. Both values are integer milliseconds.
pub trait ClockProvider {
    /// Return the current UTC Unix time in milliseconds.
    fn now_unix_ms(&self) -> Result<i64, ClockError>;

    /// Return a monotonic millisecond reading.
    fn monotonic_now_ms(&self) -> Result<u64, ClockError>;
}

/// A UTC timestamp supported by the shell date formatter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UtcDateTime {
    /// Proleptic Gregorian year in the inclusive range 0..=9999.
    pub year: u16,
    /// Month in the range 1..=12.
    pub month: u8,
    /// Day of month.
    pub day: u8,
    /// Hour in UTC.
    pub hour: u8,
    /// Minute.
    pub minute: u8,
    /// Second.
    pub second: u8,
    /// Millisecond within the second.
    pub millisecond: u16,
    epoch_ms: i64,
}

impl UtcDateTime {
    /// Convert a Unix millisecond value into a UTC date.
    pub fn from_unix_ms(epoch_ms: i64) -> Result<Self, ClockError> {
        let total_seconds = epoch_ms.div_euclid(1_000);
        let millisecond = epoch_ms.rem_euclid(1_000) as u16;
        let days = total_seconds.div_euclid(86_400);
        let seconds = total_seconds.rem_euclid(86_400);

        let z = days + 719_468;
        // `civil_from_days` needs floor division by the 400-year era length.
        // `div_euclid` already floors, so the C++ truncation adjustment
        // (`z - 146096` for negatives) must not be applied on top of it.
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
        let year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let month_prime = (5 * doy + 2).div_euclid(153);
        let day = doy - (153 * month_prime + 2).div_euclid(5) + 1;
        let month = month_prime + if month_prime < 10 { 3 } else { -9 };
        let year = year + i64::from(month <= 2);

        if !(0..=9_999).contains(&year) {
            return Err(ClockError::InvalidUnixMillis(epoch_ms));
        }

        Ok(Self {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            hour: (seconds / 3_600) as u8,
            minute: ((seconds % 3_600) / 60) as u8,
            second: (seconds % 60) as u8,
            millisecond,
            epoch_ms,
        })
    }

    /// Construct a UTC timestamp from calendar fields.
    pub fn from_calendar(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        millisecond: u16,
    ) -> Result<Self, ClockError> {
        if year > 9_999
            || !(1..=12).contains(&month)
            || day == 0
            || day > days_in_month(year as i64, month)
            || hour > 23
            || minute > 59
            || second > 59
            || millisecond > 999
        {
            return Err(ClockError::InvalidDate(format!(
                "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
            )));
        }
        let days = days_from_civil(year as i64, month, day);
        let seconds = days
            .checked_mul(86_400)
            .and_then(|value| value.checked_add(i64::from(hour) * 3_600))
            .and_then(|value| value.checked_add(i64::from(minute) * 60))
            .and_then(|value| value.checked_add(i64::from(second)))
            .ok_or_else(|| ClockError::InvalidDate("timestamp overflow".into()))?;
        let epoch_ms = seconds
            .checked_mul(1_000)
            .and_then(|value| value.checked_add(i64::from(millisecond)))
            .ok_or_else(|| ClockError::InvalidDate("timestamp overflow".into()))?;
        Self::from_unix_ms(epoch_ms)
    }

    /// Parse the legacy `WASMSH_DATE` representation.
    pub fn parse_legacy(value: &str) -> Result<Self, ClockError> {
        let fields: Vec<&str> = value.split_whitespace().collect();
        if fields.is_empty() || fields.len() > 3 {
            return Err(ClockError::InvalidDate(value.to_string()));
        }
        let date: Vec<&str> = fields[0].split('-').collect();
        if date.len() != 3 {
            return Err(ClockError::InvalidDate(value.to_string()));
        }
        let year = date[0]
            .parse::<u16>()
            .map_err(|_| ClockError::InvalidDate(value.to_string()))?;
        let month = date[1]
            .parse::<u8>()
            .map_err(|_| ClockError::InvalidDate(value.to_string()))?;
        let day = date[2]
            .parse::<u8>()
            .map_err(|_| ClockError::InvalidDate(value.to_string()))?;
        let (hour, minute, second) = if let Some(time) = fields.get(1) {
            let time: Vec<&str> = time.split(':').collect();
            if time.len() != 3 {
                return Err(ClockError::InvalidDate(value.to_string()));
            }
            (
                time[0]
                    .parse::<u8>()
                    .map_err(|_| ClockError::InvalidDate(value.to_string()))?,
                time[1]
                    .parse::<u8>()
                    .map_err(|_| ClockError::InvalidDate(value.to_string()))?,
                time[2]
                    .parse::<u8>()
                    .map_err(|_| ClockError::InvalidDate(value.to_string()))?,
            )
        } else {
            (0, 0, 0)
        };
        if let Some(zone) = fields.get(2) {
            if !zone.eq_ignore_ascii_case("utc")
                && !zone.eq_ignore_ascii_case("gmt")
                && *zone != "Z"
            {
                return Err(ClockError::InvalidDate(value.to_string()));
            }
        }
        Self::from_calendar(year, month, day, hour, minute, second, 0)
    }

    /// Return the sampled Unix time, preserving millisecond precision.
    #[must_use]
    pub fn epoch_ms(self) -> i64 {
        self.epoch_ms
    }

    /// Return Unix seconds using floor division for pre-epoch timestamps.
    #[must_use]
    pub fn epoch_seconds(self) -> i64 {
        self.epoch_ms.div_euclid(1_000)
    }

    /// Return the AWS `SigV4` date stamp (`YYYYMMDD`).
    #[must_use]
    pub fn date_stamp(self) -> String {
        format!("{:04}{:02}{:02}", self.year, self.month, self.day)
    }

    /// Return the AWS `SigV4` timestamp (`YYYYMMDDTHHMMSSZ`).
    #[must_use]
    pub fn amz_date(self) -> String {
        format!(
            "{}T{:02}{:02}{:02}Z",
            self.date_stamp(),
            self.hour,
            self.minute,
            self.second
        )
    }

    /// Return weekday with Sunday represented by zero.
    #[must_use]
    pub fn weekday_sunday_zero(self) -> usize {
        let y = i64::from(self.year);
        let m = i64::from(self.month);
        let d = i64::from(self.day);
        let (y, m) = if m < 3 { (y - 1, m + 12) } else { (y, m) };
        let dow = (d + (13 * (m + 1)) / 5 + y + y / 4 - y / 100 + y / 400) % 7;
        ((dow + 6) % 7) as usize
    }
}

/// Sample and validate a provider's wall-clock reading.
pub fn sample_utc(provider: &dyn ClockProvider) -> Result<UtcDateTime, ClockError> {
    UtcDateTime::from_unix_ms(provider.now_unix_ms()?)
}

/// A controllable clock for deterministic tests.
#[derive(Clone, Debug)]
pub struct FixedClock {
    wall_ms: Rc<Cell<i64>>,
    monotonic_ms: Rc<Cell<u64>>,
}

impl FixedClock {
    /// Create a fixed clock at a supported UTC Unix millisecond value.
    pub fn new(unix_ms: i64) -> Result<Self, ClockError> {
        UtcDateTime::from_unix_ms(unix_ms)?;
        Ok(Self {
            wall_ms: Rc::new(Cell::new(unix_ms)),
            monotonic_ms: Rc::new(Cell::new(0)),
        })
    }

    /// Set the wall clock without changing monotonic elapsed time.
    pub fn set_unix_ms(&self, unix_ms: i64) -> Result<(), ClockError> {
        UtcDateTime::from_unix_ms(unix_ms)?;
        self.wall_ms.set(unix_ms);
        Ok(())
    }

    /// Advance both clocks by a non-negative duration.
    pub fn advance_ms(&self, delta_ms: u64) -> Result<(), ClockError> {
        let wall = self
            .wall_ms
            .get()
            .checked_add(
                i64::try_from(delta_ms)
                    .map_err(|_| ClockError::InvalidUnixMillis(self.wall_ms.get()))?,
            )
            .ok_or(ClockError::InvalidUnixMillis(self.wall_ms.get()))?;
        UtcDateTime::from_unix_ms(wall)?;
        self.wall_ms.set(wall);
        self.monotonic_ms
            .set(self.monotonic_ms.get().saturating_add(delta_ms));
        Ok(())
    }
}

impl ClockProvider for FixedClock {
    fn now_unix_ms(&self) -> Result<i64, ClockError> {
        Ok(self.wall_ms.get())
    }

    fn monotonic_now_ms(&self) -> Result<u64, ClockError> {
        Ok(self.monotonic_ms.get())
    }
}

/// A clock implementation used when a WASM host has not installed one.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableClock;

impl ClockProvider for UnavailableClock {
    fn now_unix_ms(&self) -> Result<i64, ClockError> {
        Err(ClockError::Unavailable(
            "install a host callback or use explicit legacy WASMSH_DATE mode".into(),
        ))
    }

    fn monotonic_now_ms(&self) -> Result<u64, ClockError> {
        Err(ClockError::Unavailable(
            "monotonic clock is not installed".into(),
        ))
    }
}

/// Native fallback clock. WASM hosts should install their own callback.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
pub struct SystemClock {
    started: std::time::Instant,
}

#[cfg(not(target_arch = "wasm32"))]
impl SystemClock {
    /// Start a native real-time clock.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ClockProvider for SystemClock {
    fn now_unix_ms(&self) -> Result<i64, ClockError> {
        let value = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => i128::try_from(duration.as_millis()).unwrap_or(i128::MAX),
            Err(error) => -i128::try_from(error.duration().as_millis()).unwrap_or(i128::MAX),
        };
        let value = i64::try_from(value).map_err(|_| ClockError::InvalidUnixMillis(0))?;
        UtcDateTime::from_unix_ms(value).map(|_| value)
    }

    fn monotonic_now_ms(&self) -> Result<u64, ClockError> {
        Ok(self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u8) -> u8 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 {
        year / 400
    } else {
        (year - 399) / 400
    };
    let year_of_era = year - era * 400;
    let month_prime = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_epoch_and_crosses_year_boundary() {
        assert_eq!(
            UtcDateTime::from_unix_ms(0).unwrap().date_stamp(),
            "19700101"
        );
        let value = UtcDateTime::from_calendar(2024, 2, 29, 23, 59, 59, 123).unwrap();
        assert_eq!(UtcDateTime::from_unix_ms(value.epoch_ms()).unwrap(), value);
    }

    #[test]
    fn converts_early_year_zero_before_the_era_boundary() {
        // Regression: floor division of a negative `z` must not apply the
        // C++ truncation adjustment. Dates before 0000-03-01 exercise it.
        for (month, day) in [(1u8, 1u8), (1, 31), (2, 28)] {
            let value = UtcDateTime::from_calendar(0, month, day, 0, 0, 0, 0).unwrap();
            let round_trip = UtcDateTime::from_unix_ms(value.epoch_ms()).unwrap();
            assert_eq!(
                (round_trip.year, round_trip.month, round_trip.day),
                (0, month, day)
            );
        }
    }

    #[test]
    fn parses_legacy_date_and_sigv4_timestamp() {
        let value = UtcDateTime::parse_legacy("2026-01-02 03:04:05 UTC").unwrap();
        assert_eq!(value.date_stamp(), "20260102");
        assert_eq!(value.amz_date(), "20260102T030405Z");
    }

    #[test]
    fn rejects_unsupported_date_range() {
        assert!(matches!(
            UtcDateTime::from_calendar(10_000, 1, 1, 0, 0, 0, 0),
            Err(ClockError::InvalidDate(_))
        ));
    }

    #[test]
    fn fixed_clock_can_be_advanced_through_midnight() {
        let clock = FixedClock::new(
            UtcDateTime::from_calendar(2025, 12, 31, 23, 59, 59, 500)
                .unwrap()
                .epoch_ms(),
        )
        .unwrap();
        clock.advance_ms(500).unwrap();
        assert_eq!(sample_utc(&clock).unwrap().date_stamp(), "20260101");
        assert_eq!(clock.monotonic_now_ms().unwrap(), 500);
    }
}
