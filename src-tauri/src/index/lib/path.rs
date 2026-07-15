use std::collections::HashMap;

use super::scanner::ScanOutput;

const ROOT_RECORD_NUMBER: u64 = 5;

pub fn resolve_paths(output: &ScanOutput, display_limit: usize) -> Vec<(String, String)> {
    let mut record_map: HashMap<u64, (u64, &str)> = HashMap::with_capacity(output.all_records.len());
    for r in &output.all_records {
        record_map.insert(r.record_number, (r.parent_record, &r.name));
    }

    let mut cache: HashMap<u64, String> = HashMap::new();
    let mut results = Vec::new();

    for r in output.all_records.iter().take(display_limit) {
        let full_path = build_path(r.record_number, &record_map, &mut cache);
        let mtime_str = format_ntfs_time(r.mtime);
        results.push((full_path, mtime_str));
    }

    results
}

fn build_path(
    record_number: u64,
    map: &HashMap<u64, (u64, &str)>,
    cache: &mut HashMap<u64, String>,
) -> String {
    if let Some(cached) = cache.get(&record_number) {
        return cached.clone();
    }

    if record_number == ROOT_RECORD_NUMBER {
        let path = "\\".to_string();
        cache.insert(record_number, path.clone());
        return path;
    }

    if let Some(&(parent, name)) = map.get(&record_number) {
        if parent == record_number || parent == 0 {
            let path = if name.is_empty() {
                "\\".to_string()
            } else {
                format!("\\{}", name)
            };
            cache.insert(record_number, path.clone());
            return path;
        }

        let parent_path = build_path(parent, map, cache);
        let path = if parent_path == "\\" {
            format!("\\{}", name)
        } else if parent_path == "<unknown>" {
            format!("<{}>\\{}", parent, name)
        } else {
            format!("{}\\{}", parent_path, name)
        };
        cache.insert(record_number, path.clone());
        return path;
    }

    format!("<{}>", record_number)
}

pub fn format_ntfs_time(timestamp: Option<u64>) -> String {
    let ts = match timestamp {
        Some(t) if t > 0 => t,
        _ => return "N/A".to_string(),
    };

    let total_secs = ts / 10_000_000;
    let days = total_secs / 86400;
    let secs_in_day = total_secs % 86400;

    let hours = secs_in_day / 3600;
    let mins = (secs_in_day % 3600) / 60;
    let secs = secs_in_day % 60;

    let mut year = 1601u32;
    let mut remaining_days = days;

    loop {
        let days_in_year: u64 = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let month_days: [u32; 12] = [
        31,
        if is_leap_year(year) { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];

    let mut month = 1u32;
    for &md in &month_days {
        if remaining_days < md as u64 {
            break;
        }
        remaining_days -= md as u64;
        month += 1;
    }

    let day = remaining_days as u32 + 1;

    format!(
        "{:04}/{:02}/{:02} {:02}:{:02}:{:02}",
        year, month, day, hours, mins, secs
    )
}

fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_ntfs_time_epoch() {
        let result = format_ntfs_time(Some(116444736000000000));
        assert_eq!(result, "1970/01/01 00:00:00");
    }

    #[test]
    fn test_format_ntfs_time_zero() {
        assert_eq!(format_ntfs_time(Some(0)), "N/A");
        assert_eq!(format_ntfs_time(None), "N/A");
    }

    #[test]
    fn test_format_ntfs_time_y2k() {
        let result = format_ntfs_time(Some(125_911_584_000_000_000));
        assert_eq!(result, "2000/01/01 00:00:00");
    }

    #[test]
    fn test_is_leap_year() {
        assert!(is_leap_year(2000));
        assert!(!is_leap_year(1900));
        assert!(is_leap_year(2004));
        assert!(!is_leap_year(2001));
    }
}
