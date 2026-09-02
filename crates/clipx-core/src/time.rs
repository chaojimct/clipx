/// 与 WPF 版 TimeAgo 规则一致：<60s 刚刚；<60m N分钟前；<24h N小时前；否则 MM-dd HH:mm（本地时区）。
pub fn time_ago(created_ms: i64, now_ms: i64) -> String {
    let secs = ((now_ms - created_ms).max(0)) / 1000;
    if secs < 60 {
        return "刚刚".into();
    }
    if secs < 3600 {
        return format!("{}分钟前", secs / 60);
    }
    if secs < 86400 {
        return format!("{}小时前", secs / 3600);
    }
    format_local(created_ms)
}

fn format_local(ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(dt) => dt.format("%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;

    #[test]
    fn buckets() {
        let now = 2_000_000_000_000;
        assert_eq!(time_ago(now - 30_000, now), "刚刚");
        assert_eq!(time_ago(now - 5 * MIN, now), "5分钟前");
        assert_eq!(time_ago(now - 3 * HOUR, now), "3小时前");
        let old = time_ago(now - 2 * DAY, now);
        assert_eq!(old.len(), 11); // MM-dd HH:mm
        assert!(old.contains('-') && old.contains(':'));
    }
}
