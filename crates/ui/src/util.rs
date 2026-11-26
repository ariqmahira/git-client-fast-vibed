/// `YYYY-MM-DD HH:MM` for a Unix time and offset (minutes), without a date crate.
pub fn format_time(secs: i64, tz_minutes: i32) -> String {
    let t = secs + tz_minutes as i64 * 60;
    let days = t.div_euclid(86_400);
    let rem = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Howard Hinnant's days → civil date algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Resident set size of this process in bytes (Linux; `None` elsewhere).
pub fn rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Resident memory excluding a software GPU's mappings (Mesa llvmpipe /
/// lavapipe: its LLVM JIT, its driver, and Vulkan "device memory", which on
/// real GPUs lives in VRAM). This is the app's own footprint in headless
/// runs; on real hardware it's close to plain RSS.
pub fn app_rss_bytes() -> Option<u64> {
    let smaps = std::fs::read_to_string("/proc/self/smaps").ok()?;
    let mut total = 0u64;
    let mut excluded = false;
    for line in smaps.lines() {
        let first = line.split_whitespace().next().unwrap_or("");
        if first.contains('-')
            && first.chars().next().is_some_and(|c| c.is_ascii_hexdigit())
            && !first.ends_with(':')
        {
            let name = line.split_whitespace().nth(5).unwrap_or("");
            excluded = name.contains("libLLVM")
                || name.contains("libvulkan_lvp")
                || name.contains("libgallium")
                || name.starts_with("/memfd:allocation");
        } else if let Some(rest) = line.strip_prefix("Rss:") {
            if !excluded {
                total += rest.split_whitespace().next()?.parse::<u64>().ok()? * 1024;
            }
        }
    }
    Some(total)
}

/// Anonymous (heap/stack, not file-backed) resident memory excluding a
/// software GPU's mappings: memory the app itself allocated. Excludes
/// memory-mapped files such as libgit2's pack indexes (page cache the kernel
/// can drop at any time).
pub fn app_anon_bytes() -> Option<u64> {
    let smaps = std::fs::read_to_string("/proc/self/smaps").ok()?;
    let mut total = 0u64;
    let mut excluded = false;
    for line in smaps.lines() {
        let first = line.split_whitespace().next().unwrap_or("");
        if first.contains('-')
            && first.chars().next().is_some_and(|c| c.is_ascii_hexdigit())
            && !first.ends_with(':')
        {
            let name = line.split_whitespace().nth(5).unwrap_or("");
            excluded = name.starts_with("/memfd:allocation")
                || name.contains("libLLVM")
                || name.contains("libvulkan_lvp");
        } else if let Some(rest) = line.strip_prefix("Anonymous:") {
            if !excluded {
                total += rest.split_whitespace().next()?.parse::<u64>().ok()? * 1024;
            }
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates() {
        assert_eq!(format_time(0, 0), "1970-01-01 00:00");
        assert_eq!(format_time(1_577_836_800, 0), "2020-01-01 00:00");
        assert_eq!(format_time(1_577_836_800, 120), "2020-01-01 02:00");
        assert_eq!(format_time(951_782_400, 0), "2000-02-29 00:00");
        assert_eq!(format_time(-86_400, 0), "1969-12-31 00:00");
    }
}
