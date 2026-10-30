//! Viewer-local presentation of broker-owned UTC Unix millisecond metadata.
use ratatui::text::Line;
use std::{ffi::CStr, mem::MaybeUninit};

fn formatted(tm: &libc::tm, pattern: &CStr) -> Option<String> {
    let mut bytes = [0 as libc::c_char; 128];
    // SAFETY: strftime receives a valid tm and a bounded writable buffer.
    let length = unsafe { libc::strftime(bytes.as_mut_ptr(), bytes.len(), pattern.as_ptr(), tm) };
    if length == 0 {
        return None;
    }
    // SAFETY: a successful strftime writes a NUL-terminated string.
    unsafe { CStr::from_ptr(bytes.as_ptr()) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

fn local_time(milliseconds: i64) -> Option<libc::tm> {
    if milliseconds < 0 {
        return None;
    }
    let seconds: libc::time_t = milliseconds / 1000;
    let mut local = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r initializes the output on success. It uses the
    // viewer process's TZ/system timezone, including daylight-saving rules.
    if unsafe { libc::localtime_r(&seconds, local.as_mut_ptr()) }.is_null() {
        return None;
    }
    Some(unsafe { local.assume_init() })
}

fn local_zone(local: &libc::tm) -> Option<String> {
    let abbreviation = formatted(local, c"%Z")?;
    let zone = if !abbreviation.is_empty()
        && abbreviation.len() <= 16
        && abbreviation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-:".contains(&byte))
    {
        abbreviation
    } else {
        let offset = formatted(local, c"%z")?;
        if offset.len() != 5
            || !matches!(offset.as_bytes()[0], b'+' | b'-')
            || !offset.as_bytes()[1..].iter().all(u8::is_ascii_digit)
        {
            return None;
        }
        format!("UTC{}:{}", &offset[..3], &offset[3..])
    };
    Some(zone)
}

fn local_parts(milliseconds: i64) -> Option<(String, String, String, String)> {
    let local = local_time(milliseconds)?;
    let zone = local_zone(&local)?;
    Some((
        formatted(&local, c"%a %d %b %H:%M")?,
        formatted(&local, c"%d %b %H:%M")?,
        formatted(&local, c"%H:%M")?,
        zone,
    ))
}

/// Present archive/deletion dates with a year in the viewer's local timezone.
/// Missing or invalid broker times remain unknown instead of using read time.
pub(crate) fn archive_date(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(local_time)
        .and_then(|local| {
            Some(format!(
                "{} {}",
                formatted(&local, c"%d %b %Y %H:%M")?,
                local_zone(&local)?
            ))
        })
        .unwrap_or_else(|| "unknown".into())
}

pub(crate) fn footer(timestamp: Option<i64>, width: u16) -> String {
    let candidates = match timestamp.and_then(local_parts) {
        Some((full, short, clock, zone)) => vec![
            format!("Last Updated: {full} {zone}"),
            format!("Updated: {short} {zone}"),
            format!("Updated: {clock} {zone}"),
            format!("{clock} {zone}"),
            "Updated".into(),
            "Upd".into(),
            "·".into(),
        ],
        None => vec![
            "Last Updated: unknown".into(),
            "Updated: unknown".into(),
            "Updated: ?".into(),
            "?".into(),
        ],
    };
    candidates
        .into_iter()
        .find(|text| Line::from(text.as_str()).width() <= usize::from(width))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_footers_never_exceed_terminal_display_width() {
        for timestamp in [None, Some(-1), Some(1_768_435_200_000)] {
            for width in 0..100 {
                let value = footer(timestamp, width);
                assert!(Line::from(value).width() <= usize::from(width));
            }
        }
        assert_eq!(footer(None, 0), "");
        assert_eq!(footer(None, 1), "?");
        assert_eq!(footer(None, 80), "Last Updated: unknown");
    }

    #[test]
    fn unknown_archive_dates_do_not_invent_an_instant() {
        assert_eq!(archive_date(None), "unknown");
        assert_eq!(archive_date(Some(-1)), "unknown");
    }

    #[test]
    fn local_timezone_and_dst_are_resolved_in_isolated_viewer_processes() {
        // TZ is set before each child starts; never mutate global environment
        // while other Rust tests or native process-spawn fixtures are running.
        for zone in ["UTC", "Australia/Sydney"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "board_time::tests::timezone_probe",
                    "--nocapture",
                ])
                .env("TZ", zone)
                .env("AGB18_TIME_PROBE", zone)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{zone}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    #[ignore = "subprocess entry point invoked by the timezone/DST parent test"]
    fn timezone_probe() {
        let zone = std::env::var("AGB18_TIME_PROBE").unwrap();
        let summer = footer(Some(1_768_435_200_000), 100);
        let winter = footer(Some(1_784_073_600_000), 100);
        let archive_summer = archive_date(Some(1_768_435_200_000));
        let archive_winter = archive_date(Some(1_784_073_600_000));
        if zone == "Australia/Sydney" {
            assert_eq!(archive_summer, "15 Jan 2026 11:00 AEDT");
            assert_eq!(archive_winter, "15 Jul 2026 10:00 AEST");
            assert!(summer.contains("15 Jan 11:00 AEDT"), "{summer}");
            assert!(winter.contains("15 Jul 10:00 AEST"), "{winter}");
        } else {
            assert_eq!(zone, "UTC");
            assert_eq!(archive_summer, "15 Jan 2026 00:00 UTC");
            assert_eq!(archive_winter, "15 Jul 2026 00:00 UTC");
            assert!(summer.contains("15 Jan 00:00 UTC"), "{summer}");
            assert!(winter.contains("15 Jul 00:00 UTC"), "{winter}");
        }
    }
}
