//! 按项目的数据库连接配置：一个项目一份，环境用 profile 区分。
//!
//! 为什么不放进项目仓库：主机、账号、库名都随机器走，不同机器本来就不一样；
//! 放进 `$YI_HOME/projects/<编码路径>/` 既不污染工作仓库，也不用担心把内网
//! 地址提交上去。密码永远只在环境变量里，文件里出现 `password` 直接报错。
//!
//! 多个项目共用一个库时，项目文件只写 `{"use": "名字"}`，真正的配置放在
//! `$YI_HOME/db/<名字>.json`；引用只允许一层，共享文件里不能再写 `use`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::{Error, Result};

/// 一个环境（dev / test / prod）的连接参数。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub database: String,
    /// 密码所在的环境变量名；省略时用 `MYSQL_PWD`。
    #[serde(default)]
    pub password_env: Option<String>,
}

fn default_port() -> u16 {
    3306
}

/// 一个项目的全部环境。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    /// 没显式选过时用哪个 profile。
    pub default: String,
    pub profiles: BTreeMap<String, Profile>,
}

/// 项目文件里只写这一行，指向共享配置。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UseRef {
    #[serde(rename = "use")]
    name: String,
}

/// 读出来的结果：生效的配置、它的路径，以及它是不是共享配置。
#[derive(Debug)]
pub struct Loaded {
    pub config: DbConfig,
    pub path: PathBuf,
    /// 共享配置的名字；`None` 表示项目自带。
    pub shared: Option<String>,
}

/// 项目配置文件的位置：和会话文件共用同一套「按项目编码」的目录。
pub fn path(home: &Path, cwd: &str) -> PathBuf {
    home.join("projects")
        .join(crate::config::encode_project(cwd))
        .join("db.json")
}

/// 共享配置的位置：`$YI_HOME/db/<名字>.json`。
pub fn shared_path(home: &Path, name: &str) -> Result<PathBuf> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if !valid {
        return Err(Error::Config(format!(
            "共享配置名 `{name}` 只能用字母、数字、- 和 _"
        )));
    }
    Ok(home.join("db").join(format!("{name}.json")))
}

/// 读配置；文件不存在时返回 `None`，不是错误。
pub fn load(home: &Path, cwd: &str) -> Result<Option<Loaded>> {
    let file = path(home, cwd);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(Error::Config(format!(
                "读不了数据库配置 {}：{err}",
                file.display()
            )));
        }
    };
    let raw: Value = parse_json(&text, &file.to_string_lossy())?;

    if raw.get("use").is_some() {
        let reference: UseRef = serde_json::from_value(raw).map_err(|err| {
            Error::Config(format!(
                "{}：写成 {{\"use\": \"名字\"}} 时不能再带别的字段（{err}）",
                file.display()
            ))
        })?;
        let shared = shared_path(home, &reference.name)?;
        let text = std::fs::read_to_string(&shared)
            .map_err(|err| Error::Config(format!("共享配置读不了 {}：{err}", shared.display())))?;
        let config = parse_config(
            parse_json(&text, &shared.to_string_lossy())?,
            &shared.to_string_lossy(),
            true,
        )?;
        return Ok(Some(Loaded {
            config,
            path: shared,
            shared: Some(reference.name),
        }));
    }

    let config = parse_config(raw, &file.to_string_lossy(), false)?;
    Ok(Some(Loaded {
        config,
        path: file,
        shared: None,
    }))
}

fn parse_json(text: &str, label: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|err| Error::Config(format!("{label} 不是合法 JSON：{err}")))
}

/// `shared` 为真时在读共享文件：那里不允许再出现 `use`。
fn parse_config(raw: Value, label: &str, shared: bool) -> Result<DbConfig> {
    if shared && raw.get("use").is_some() {
        return Err(Error::Config(format!(
            "{label}：共享配置里不能再写 use，引用只允许一层"
        )));
    }
    if let Some(profiles) = raw.get("profiles").and_then(Value::as_object) {
        for (name, profile) in profiles {
            if profile.get("password").is_some() {
                return Err(Error::Config(format!(
                    "{label}：profile `{name}` 里写了 password；密码只从环境变量读，请改成 password_env"
                )));
            }
        }
    }
    let config: DbConfig = serde_json::from_value(raw)
        .map_err(|err| Error::Config(format!("{label} 的结构不对：{err}")))?;
    if config.profiles.is_empty() {
        return Err(Error::Config(format!("{label}：profiles 是空的")));
    }
    if !config.profiles.contains_key(&config.default) {
        return Err(Error::Config(format!(
            "{label}：default 指向的 `{}` 不在 profiles 里",
            config.default
        )));
    }
    Ok(config)
}

/// 定下这次连哪个 profile：显式选过就用它，否则用配置里的 default。
pub fn resolve<'a>(config: &'a DbConfig, chosen: Option<&str>) -> Result<(String, &'a Profile)> {
    let name = chosen.unwrap_or(&config.default).to_string();
    match config.profiles.get(&name) {
        Some(profile) => Ok((name, profile)),
        None => Err(Error::Config(format!(
            "没有叫 `{name}` 的 profile；可选：{}",
            config
                .profiles
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join("、")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SAMPLE: &str = r#"{
        "default": "dev",
        "profiles": {
            "dev": { "host": "127.0.0.1", "user": "fm", "database": "fm_dev" },
            "test": { "host": "10.0.0.8", "port": 3307, "user": "fm", "database": "fm_test",
                      "password_env": "MYSQL_PWD_FM_TEST" }
        }
    }"#;

    fn parse(text: &str) -> Result<DbConfig> {
        parse_config(parse_json(text, "test")?, "test", false)
    }

    fn temp_home(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("yi-db-{tag}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("有父目录")).expect("建目录");
        std::fs::write(path, text).expect("写文件");
    }

    #[test]
    fn a_profile_defaults_to_the_standard_port() {
        let config = parse(SAMPLE).expect("能解析");
        let (name, profile) = resolve(&config, None).expect("有默认");
        assert_eq!(name, "dev");
        assert_eq!(profile.port, 3306);
        assert_eq!(profile.database, "fm_dev");
    }

    #[test]
    fn an_explicit_profile_wins_and_keeps_its_password_variable() {
        let config = parse(SAMPLE).expect("能解析");
        let (name, profile) = resolve(&config, Some("test")).expect("能选中");
        assert_eq!(name, "test");
        assert_eq!(profile.port, 3307);
        assert_eq!(profile.password_env.as_deref(), Some("MYSQL_PWD_FM_TEST"));
    }

    #[test]
    fn a_missing_default_or_unknown_profile_is_an_error() {
        let broken = r#"{ "default": "prod", "profiles": { "dev": { "host": "h", "user": "u", "database": "d" } } }"#;
        assert!(parse(broken).is_err(), "default 指向不存在的 profile");
        let config = parse(SAMPLE).expect("能解析");
        assert!(
            resolve(&config, Some("prod")).is_err(),
            "选不存在的 profile"
        );
    }

    #[test]
    fn a_password_in_the_file_is_refused() {
        let leaky = r#"{ "default": "dev", "profiles": { "dev": { "host": "h", "user": "u", "database": "d", "password": "x" } } }"#;
        let err = parse(leaky).expect_err("必须拒绝");
        assert!(err.to_string().contains("password_env"), "{err}");
    }

    #[test]
    fn a_typo_in_a_field_name_is_refused() {
        let typo = r#"{ "default": "dev", "profiles": { "dev": { "host": "h", "user": "u", "databse": "d" } } }"#;
        assert!(parse(typo).is_err(), "字段名拼错要报出来");
    }

    #[test]
    fn a_project_can_point_at_a_shared_config() {
        let home = temp_home("shared");
        let cwd = "/tmp/shared-proj";
        write(&home.join("db/fm.json"), SAMPLE);
        write(&path(&home, cwd), r#"{ "use": "fm" }"#);

        let loaded = load(&home, cwd).expect("能读").expect("有配置");
        assert_eq!(loaded.shared.as_deref(), Some("fm"));
        assert_eq!(loaded.path, home.join("db/fm.json"));
        assert_eq!(loaded.config.default, "dev");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_missing_shared_config_names_the_path() {
        let home = temp_home("missing-shared");
        let cwd = "/tmp/shared-missing";
        write(&path(&home, cwd), r#"{ "use": "nope" }"#);

        let err = load(&home, cwd).expect_err("必须报错");
        assert!(err.to_string().contains("db/nope.json"), "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_shared_config_cannot_reference_another() {
        let home = temp_home("nested");
        let cwd = "/tmp/shared-nested";
        write(&home.join("db/fm.json"), r#"{ "use": "other" }"#);
        write(&path(&home, cwd), r#"{ "use": "fm" }"#);

        let err = load(&home, cwd).expect_err("必须报错");
        assert!(err.to_string().contains("只允许一层"), "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_project_file_cannot_mix_use_with_other_fields() {
        let home = temp_home("mixed");
        let cwd = "/tmp/shared-mixed";
        write(
            &path(&home, cwd),
            r#"{ "use": "fm", "default": "dev", "profiles": {} }"#,
        );

        assert!(load(&home, cwd).is_err(), "use 和完整配置不能混写");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_shared_name_cannot_escape_the_directory() {
        let home = temp_home("escape");
        let cwd = "/tmp/shared-escape";
        write(&path(&home, cwd), r#"{ "use": "../secrets" }"#);

        let err = load(&home, cwd).expect_err("必须拒绝");
        assert!(err.to_string().contains("只能用字母"), "{err}");
        std::fs::remove_dir_all(&home).ok();
    }
}
