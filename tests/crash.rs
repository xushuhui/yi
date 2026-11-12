//! 崩溃钩子：panic 之后，会话文件里要留下原因和位置。

mod common;

use common::*;

#[test]
fn a_panic_writes_a_notice_into_the_session_file() {
    let home = temp_home("crash-home");
    let path = session_path(&home);
    std::fs::create_dir_all(path.parent().expect("目录")).expect("建目录");
    std::fs::write(&path, "").expect("建文件");

    yi::crash::install_hook();
    yi::crash::install(&path, 3);

    let caught = std::panic::catch_unwind(|| {
        panic!("测试用的崩溃原因");
    });
    assert!(caught.is_err(), "panic 应当被捕获，供测试继续");

    let text = std::fs::read_to_string(&path).expect("读会话文件");
    let entry: serde_json::Value = serde_json::from_str(text.trim()).expect("合法 JSON");
    assert_eq!(entry["type"], "notice", "{entry}");
    assert_eq!(entry["seq"], 4, "接在最后一条之后：{entry}");
    let note = entry["text"].as_str().unwrap_or("");
    assert!(note.contains("测试用的崩溃原因"), "{entry}");
    assert!(note.contains("tests/crash.rs"), "要带位置：{entry}");
}
