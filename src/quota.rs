//! opencode-go 的额度窗口：5 小时 / 7 天 / 每月。
//!
//! 数据源是 `GET {base}/v1/usage`——omp 的 `packages/ai/src/usage/opencode-go.ts`
//! 用同一个接口：`usage.rolling/weekly/monthly` 各带 `percent`（已用百分比）、
//! `status`、`resetsAt`（ISO 时间）。
//!
//! 后台线程每 5 分钟刷新一次，页脚只读缓存：额度是锦上添花的信息，网络慢或
//! 接口报错时宁可不显示，也不能拖住一轮对话。只有 opencode-go 会请求；
//! `/switch` 换到别的供应商后页脚不再显示，缓存保留但不使用。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::Config;

/// 刷新间隔：额度的计费窗口以小时计，5 分钟足够跟上用量变化。
const REFRESH: Duration = Duration::from_secs(300);

/// 接口字段 → 页脚缩写，顺序固定。
const WINDOWS: [(&str, &str); 3] = [("rolling", "5h"), ("weekly", "7d"), ("monthly", "mo")];

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub label: &'static str,
    /// 已用百分比（0–100，取整）。
    pub percent: u32,
    /// 重置时刻（毫秒时间戳）；接口没给或解析不了就是 `None`。
    pub resets_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Quota {
    pub windows: Vec<Window>,
}

impl Quota {
    /// 页脚后缀；每次按“现在”重算剩余时间，缓存里的是重置时刻。
    pub fn suffix(&self, now_ms: u64) -> String {
        let mut out = String::new();
        for window in &self.windows {
            out.push_str(&format!(" · {} {}%", window.label, window.percent));
            if let Some(resets_at) = window.resets_at {
                out.push_str(&format!(" ({})", compact(resets_at.saturating_sub(now_ms))));
            }
        }
        out
    }
}

static CACHE: OnceLock<Mutex<Option<Quota>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Quota>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 启动后台刷新；不是 opencode-go 就什么都不做。
pub fn start(cfg: &Config, session: &str) {
    if cfg.provider != "opencode-go" {
        return;
    }
    let url = usage_url(&cfg.base_url);
    let key = cfg.api_key.clone();
    let session = session.to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(cfg.proxy.clone())
        .user_agent(format!("yi/{}", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_per_read(Some(Duration::from_secs(10)))
        .build()
        .into();
    std::thread::spawn(move || {
        loop {
            if let Some(quota) = fetch(&agent, &url, &key, &session)
                && let Ok(mut current) = slot().lock()
            {
                *current = Some(quota);
            }
            std::thread::sleep(REFRESH);
        }
    });
}

/// 页脚后缀，例如 ` · 5h 4% (2h 57m) · 7d 16% (2d 8h) · mo 44% (14d 19h)`。
///
/// 每次都按“现在”重算剩余时间；缓存里的是重置时刻，不是倒计时。
pub fn suffix(now_ms: u64) -> Option<String> {
    let quota = slot().lock().ok()?.clone()?;
    Some(quota.suffix(now_ms))
}

/// 当前时间（毫秒），测试也用它注入固定时刻。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `https://opencode.ai/zen/go/v1` → `https://opencode.ai/zen/go/v1/usage`。
fn usage_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = base
        .strip_suffix("/v1")
        .or_else(|| base.strip_suffix("/V1"))
        .unwrap_or(base);
    format!("{base}/v1/usage")
}

fn fetch(agent: &ureq::Agent, url: &str, key: &str, session: &str) -> Option<Quota> {
    let response = agent
        .get(url)
        .header("accept", "application/json")
        .header("authorization", &format!("Bearer {key}"))
        .header("x-opencode-session", session)
        .call()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = response.into_body().read_to_string().ok()?;
    let body: Value = serde_json::from_str(&text).ok()?;
    parse(&body)
}

/// 三个窗口必须齐全且合法才显示：缺一个说明接口变了，宁可整段不显示。
fn parse(body: &Value) -> Option<Quota> {
    let usage = body.get("usage")?;
    let mut windows = Vec::new();
    for (field, label) in WINDOWS {
        let raw = usage.get(field)?;
        let percent = raw.get("percent")?.as_f64()?;
        if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
            return None;
        }
        // omp 只接受这两个状态；未知状态按接口变化处理。
        let status = raw.get("status").and_then(Value::as_str).unwrap_or("ok");
        if !matches!(status, "ok" | "rate-limited") {
            return None;
        }
        let resets_at = raw
            .get("resetsAt")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms);
        windows.push(Window {
            label,
            percent: percent.round() as u32,
            resets_at,
        });
    }
    Some(Quota { windows })
}

/// 剩余时间压成两个单位：超过一天用「天 + 小时」，否则「小时 + 分钟」，
/// 不足一小时只给分钟。和 omp 页脚的 `(2d 8h)` / `(2h 57m)` 一致。
fn compact(ms: u64) -> String {
    let minutes = ms / 60_000;
    let days = minutes / 1440;
    let hours = (minutes % 1440) / 60;
    let mins = minutes % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

/// 只认 ISO 8601 / RFC 3339 的 `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)`。
/// 不引日期库：这里只需要一个固定格式，二十行足够，多一个直接依赖不划算。
fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut index = 19;
    let mut millis = 0u64;
    if bytes.get(index) == Some(&b'.') {
        let start = index + 1;
        index = start;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index > start {
            let digits = &text[start..index];
            let mut padded: String = digits.chars().take(3).collect();
            while padded.len() < 3 {
                padded.push('0');
            }
            millis = padded.parse().ok()?;
        }
    }
    let offset = match bytes.get(index) {
        Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let hour = number(index + 1..index + 3)?;
            let minute = number(index + 4..index + 6)?;
            let seconds = hour * 3600 + minute * 60;
            if *sign == b'+' { seconds } else { -seconds }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month as u32, day as u32);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok().map(|s| s * 1000 + millis)
}

/// 公历日期 → 1970-01-01 起的天数（Howard Hinnant 的算法，负年份也成立）。
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime as i64 + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        serde_json::json!({
            "usage": {
                "rolling": {"percent": 4, "status": "ok", "resetsAt": "2026-11-27T05:11:04.000Z"},
                "weekly": {"percent": 16.4, "status": "ok", "resetsAt": "2026-11-29T10:14:04Z"},
                "monthly": {"percent": 44, "status": "rate-limited", "resetsAt": "2026-12-11T21:14:04Z"}
            }
        })
    }

    #[test]
    fn the_usage_url_handles_a_base_with_or_without_v1() {
        assert_eq!(
            usage_url("https://opencode.ai/zen/go/v1"),
            "https://opencode.ai/zen/go/v1/usage"
        );
        assert_eq!(
            usage_url("https://opencode.ai/zen/go/v1/"),
            "https://opencode.ai/zen/go/v1/usage"
        );
        assert_eq!(
            usage_url("https://opencode.ai/zen/go"),
            "https://opencode.ai/zen/go/v1/usage"
        );
    }

    #[test]
    fn rfc3339_parses_seconds_millis_and_offsets() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.500Z"), Some(500));
        assert_eq!(
            parse_rfc3339_ms("1970-01-01T08:00:00+08:00"),
            Some(0),
            "东八区的零点就是 UTC 零点"
        );
        assert_eq!(parse_rfc3339_ms("不是时间"), None);
    }

    #[test]
    fn the_footer_shows_used_percent_and_two_unit_countdowns() {
        let quota = parse(&body()).expect("合法响应");
        let now = parse_rfc3339_ms("2026-11-27T02:14:04Z").unwrap();
        assert_eq!(
            quota.suffix(now),
            " · 5h 4% (2h 57m) · 7d 16% (2d 8h) · mo 44% (14d 19h)"
        );
    }

    #[test]
    fn a_partial_or_malformed_response_is_ignored() {
        let mut value = body();
        value["usage"].as_object_mut().unwrap().remove("weekly");
        assert_eq!(parse(&value), None);

        let mut value = body();
        value["usage"]["rolling"]["percent"] = serde_json::json!(140);
        assert_eq!(parse(&value), None);
    }

    #[test]
    fn compact_duration_switches_units_at_a_day() {
        assert_eq!(compact(0), "0m");
        assert_eq!(compact(59_000), "0m");
        assert_eq!(compact(59 * 60_000), "59m");
        assert_eq!(compact(60 * 60_000), "1h 0m");
        assert_eq!(compact((23 * 60 + 59) * 60_000), "23h 59m");
        assert_eq!(compact(24 * 60 * 60_000), "1d 0h");
    }
}
