//! Mac 1904-epoch timestamps.

/// Seconds between the Mac epoch (1904-01-01 00:00:00) and the Unix epoch
/// (1970-01-01 00:00:00). 66 years, 17 of which were leap years.
const MAC_EPOCH_TO_UNIX: i64 = 2_082_844_800;

/// A classic Mac OS timestamp: seconds since 1904-01-01 00:00:00.
///
/// The classic Mac had no notion of time zones — the date/time stored on disk
/// is whatever the creating machine's clock read, i.e. nominally *local* time.
/// This type performs **no** timezone correction: [`to_unix`](Self::to_unix)
/// and [`to_ymd_hms`](Self::to_ymd_hms) treat the stored value as if it were
/// UTC, so a timestamp read from a disk formatted in another zone renders as
/// the wall-clock time that machine saw. That is the best available
/// interpretation; the on-disk format simply does not record the offset.
///
/// The `u32` range runs from 1904-01-01 to 2040-02-06.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MacTimestamp(pub u32);

impl MacTimestamp {
    /// Seconds since the Unix epoch. Values before 1970 are negative.
    pub fn to_unix(self) -> i64 {
        self.0 as i64 - MAC_EPOCH_TO_UNIX
    }

    /// Convert from a Unix timestamp, or `None` if the instant falls outside
    /// the representable range (before 1904-01-01 or after 2040-02-06).
    pub fn from_unix(unix: i64) -> Option<Self> {
        let mac = unix.checked_add(MAC_EPOCH_TO_UNIX)?;
        if (0..=u32::MAX as i64).contains(&mac) {
            Some(MacTimestamp(mac as u32))
        } else {
            None
        }
    }

    /// The current time, saturating at the ends of the representable range
    /// (so a wildly wrong system clock yields a clamped value, never a panic).
    pub fn now() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unix = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs().min(i64::MAX as u64) as i64,
            // Clock is before 1970.
            Err(e) => -(e.duration().as_secs().min(i64::MAX as u64) as i64),
        };
        let mac = unix
            .saturating_add(MAC_EPOCH_TO_UNIX)
            .clamp(0, u32::MAX as i64);
        MacTimestamp(mac as u32)
    }

    /// Break the timestamp down into `(year, month, day, hour, minute, second)`
    /// in the proleptic Gregorian calendar, with no timezone adjustment.
    ///
    /// Uses Howard Hinnant's `civil_from_days` algorithm, which shifts the year
    /// so it begins in March; that puts the leap day at the end of the year and
    /// removes every special case from the month-length arithmetic.
    pub fn to_ymd_hms(self) -> (i32, u32, u32, u32, u32, u32) {
        let unix = self.to_unix();
        let days = unix.div_euclid(86_400);
        let secs_of_day = unix.rem_euclid(86_400);

        // civil_from_days: days since 1970-01-01 -> (y, m, d).
        let z = days + 719_468; // shift epoch to 0000-03-01
        let era = z.div_euclid(146_097); // 400-year era
        let doe = z.rem_euclid(146_097); // day of era, [0, 146096]
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // March-based month, [0, 11]
        let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
        let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
        let y = y + i64::from(m <= 2); // back to January-based years

        (
            y as i32,
            m as u32,
            d as u32,
            (secs_of_day / 3_600) as u32,
            (secs_of_day / 60 % 60) as u32,
            (secs_of_day % 60) as u32,
        )
    }
}

impl std::fmt::Display for MacTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (y, mo, d, h, mi, s) = self.to_ymd_hms();
        write!(f, "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_epoch() {
        assert_eq!(MacTimestamp(0).to_ymd_hms(), (1904, 1, 1, 0, 0, 0));
        assert_eq!(MacTimestamp(0).to_unix(), -MAC_EPOCH_TO_UNIX);
    }

    #[test]
    fn unix_epoch() {
        let t = MacTimestamp(2_082_844_800);
        assert_eq!(t.to_ymd_hms(), (1970, 1, 1, 0, 0, 0));
        assert_eq!(t.to_unix(), 0);
    }

    #[test]
    fn modern_value() {
        // unix 1_577_836_800 == 2020-01-01 00:00:00
        let t = MacTimestamp(3_660_681_600);
        assert_eq!(t.to_unix(), 1_577_836_800);
        assert_eq!(t.to_ymd_hms(), (2020, 1, 1, 0, 0, 0));
    }

    #[test]
    fn time_of_day_and_leap_days() {
        // 1984-01-24 12:34:56, the Mac's introduction.
        let t = MacTimestamp::from_unix(443_795_696).unwrap();
        assert_eq!(t.to_ymd_hms(), (1984, 1, 24, 12, 34, 56));
        // 2000 was a leap year (divisible by 400); 1900 was not.
        assert_eq!(
            MacTimestamp::from_unix(951_782_400).unwrap().to_ymd_hms(),
            (2000, 2, 29, 0, 0, 0)
        );
        // Last second of a year.
        assert_eq!(
            MacTimestamp::from_unix(1_609_459_199).unwrap().to_ymd_hms(),
            (2020, 12, 31, 23, 59, 59)
        );
    }

    #[test]
    fn from_unix_round_trip() {
        for &u in &[
            -MAC_EPOCH_TO_UNIX,
            -1,
            0,
            1,
            1_577_836_800,
            u32::MAX as i64 - MAC_EPOCH_TO_UNIX,
        ] {
            let t = MacTimestamp::from_unix(u).expect("in range");
            assert_eq!(t.to_unix(), u);
            assert_eq!(MacTimestamp::from_unix(t.to_unix()), Some(t));
        }
    }

    #[test]
    fn from_unix_out_of_range() {
        assert_eq!(MacTimestamp::from_unix(-MAC_EPOCH_TO_UNIX - 1), None);
        assert_eq!(
            MacTimestamp::from_unix(u32::MAX as i64 - MAC_EPOCH_TO_UNIX + 1),
            None
        );
        assert_eq!(MacTimestamp::from_unix(i64::MAX), None);
        assert_eq!(MacTimestamp::from_unix(i64::MIN), None);
    }

    #[test]
    fn display_format() {
        assert_eq!(MacTimestamp(0).to_string(), "1904-01-01 00:00:00");
        assert_eq!(
            MacTimestamp(2_082_844_800).to_string(),
            "1970-01-01 00:00:00"
        );
        assert_eq!(
            MacTimestamp::from_unix(443_795_696).unwrap().to_string(),
            "1984-01-24 12:34:56"
        );
        assert_eq!(MacTimestamp(u32::MAX).to_string(), "2040-02-06 06:28:15");
    }

    #[test]
    fn now_is_plausible() {
        // Anything after 2024 and inside the u32 range.
        let now = MacTimestamp::now();
        assert!(now.to_unix() > 1_704_067_200, "now() = {now}");
        assert!(now.0 < u32::MAX);
    }

    #[test]
    fn ordering_is_chronological() {
        assert!(MacTimestamp(0) < MacTimestamp(1));
        let mut v = [MacTimestamp(5), MacTimestamp(1), MacTimestamp(3)];
        v.sort();
        assert_eq!(v, [MacTimestamp(1), MacTimestamp(3), MacTimestamp(5)]);
    }
}
