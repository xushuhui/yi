mod common;

use std::time::{Duration, Instant};

use common::{config, raw_json_response, start_server, temp_home};

#[test]
fn the_footer_quota_is_fetched_from_the_usage_endpoint() {
    let body = serde_json::json!({
        "usage": {
            "rolling": {"status": "ok", "percent": 4, "resetsAt": "2026-11-27T05:11:04.000Z"},
            "weekly": {"status": "ok", "percent": 16, "resetsAt": "2026-11-29T10:14:04.000Z"},
            "monthly": {"status": "ok", "percent": 44, "resetsAt": "2026-12-11T21:14:04.000Z"}
        }
    })
    .to_string();
    let server = start_server(vec![raw_json_response(&body)]);
    let home = temp_home("quota-home");
    let mut cfg = config(server.port, &home, None);
    cfg.provider = "opencode-go";
    cfg.base_url = format!("http://127.0.0.1:{}/v1", server.port);
    cfg.api_key = "test-key".to_string();

    yi::quota::start(&cfg, "test-session");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(text) = yi::quota::suffix(1_795_745_644_000) {
            assert_eq!(
                text, " · 5h 4% (2h 57m) · 7d 16% (2d 8h) · mo 44% (14d 19h)",
                "{text}"
            );
            return;
        }
        assert!(Instant::now() < deadline, "后台线程没有取到额度");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_non_opencode_provider_never_requests_usage() {
    // 先起一个假服务器，只等一个请求：如果代码真的发了，它会收到连接。
    let server = start_server(vec![raw_json_response("{}")]);
    let home = temp_home("quota-skip-home");
    let mut cfg = config(server.port, &home, None);
    cfg.provider = "deepseek";
    cfg.base_url = format!("http://127.0.0.1:{}/v1", server.port);
    cfg.api_key = "test-key".to_string();
    yi::quota::start(&cfg, "test-session");
    std::thread::sleep(Duration::from_millis(150));
    assert!(server.headers.lock().is_empty(), "不该发出额度请求");
}
