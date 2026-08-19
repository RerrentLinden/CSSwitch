pub mod anthropic_compat;
pub(crate) mod anthropic_sse;
pub mod auth;
pub mod config;
pub mod connect;
pub mod control;
pub mod deepseek_compat;
pub mod kimi_search_noise;
pub mod messages;
pub mod models;
pub mod official_passthrough;
pub mod profile;
pub(crate) mod provider_contracts;
pub mod science;
pub mod server;
pub mod static_profile;

/// serve 路径的服务日志行:每行带 UTC 毫秒时间戳,使 `~/.csswitch/service.log`
/// 可与 Science 侧日志直接对齐(此前无时间戳,只能靠进程启动时刻反推)。
#[macro_export]
macro_rules! log_line {
    ($($arg:tt)*) => {
        eprintln!("[{}] {}", $crate::utc_timestamp_ms(), format_args!($($arg)*))
    };
}

/// 当前 UTC 时刻的 ISO-8601 毫秒时间戳,如 `2026-08-19T12:19:46.558Z`。
pub fn utc_timestamp_ms() -> String {
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    format_utc_timestamp_ms(epoch_ms)
}

/// 固定 epoch 毫秒 → 固定字符串,std-only(不引时间类第三方依赖)。
pub fn format_utc_timestamp_ms(epoch_ms: u128) -> String {
    let millis = (epoch_ms % 1_000) as u32;
    let secs = (epoch_ms / 1_000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3_600,
        (tod % 3_600) / 60,
        tod % 60
    )
}

/// Howard Hinnant 的 civil_from_days:epoch 起的天数 → 公历 (年, 月, 日)。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::format_utc_timestamp_ms;

    #[test]
    fn timestamp_format_matches_known_vectors() {
        // 已知向量:epoch 起点、闰日、以及一个毫秒非零的近期时刻
        // (1_787_141_986 由 `date -u -r` 独立验证为 2026-08-19T12:19:46Z)。
        assert_eq!(format_utc_timestamp_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_utc_timestamp_ms(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
        assert_eq!(
            format_utc_timestamp_ms(1_787_141_986_558),
            "2026-08-19T12:19:46.558Z"
        );
    }
}
