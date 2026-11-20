//! 按项目的数据库连接配置：一个项目一份，环境用 profile 区分。
//!
//! 为什么不放进项目仓库：主机、账号、库名都随机器走，不同机器本来就不一样；
//! 放进 `$YI_HOME/projects/<编码路径>/` 既不污染工作仓库，也不用担心把内网
//! 地址提交上去。密码永远只在环境变量里，文件里出现 `password` 直接报错。

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

/// 配置文件的位置：和会话文件共用同一套「按项目编码」的目录。
pub fn path(home: &Path, cwd: &str) -> PathBuf {
    home.join("projects")
        .join(crate::config::encode_project(cwd))
        .join("db.json")
}

/// 读配置；文件不存在时返回 `None`，不是错误。
pub fn load(home: &Path, cwd: &str) -> Result<Option<DbConfig>> {
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
    from_json(&text)
        .map(Some)
        .map_err(|err| Error::Config(format!("{}：{err}", file.display())))
}

fn from_json(text: &str) -> Result<DbConfig> {
    let raw: Value = serde_json::from_str(text)
        .map_err(|err| Error::Config(format!("数据库配置不是合法 JSON：{err}")))?;
    if let Some(profiles) = raw.get("profiles").and_then(Value::as_object) {
        for (name, profile) in profiles {
            if profile.get("password").is_some() {
                return Err(Error::Config(format!(
                    "profile `{name}` 里写了 password；密码只从环境变量读，请改成 password_env"
                )));
            }
        }
    }
    let config: DbConfig = serde_json::from_value(raw)
        .map_err(|err| Error::Config(format!("数据库配置的结构不对：{err}")))?;
    if config.profiles.is_empty() {
        return Err(Error::Config("profiles 是空的".to_string()));
    }
    if !config.profiles.contains_key(&config.default) {
        return Err(Error::Config(format!(
            "default 指向的 `{}` 不在 profiles 里",
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

    const SAMPLE: &str = r#"{
        "default": "dev",
        "profiles": {
            "dev": { "host": "127.0.0.1", "user": "fm", "database": "fm_dev" },
            "test": { "host": "10.0.0.8", "port": 3307, "user": "fm", "database": "fm_test",
                      "password_env": "MYSQL_PWD_FM_TEST" }
        }
    }"#;

    #[test]
    fn a_profile_defaults_to_the_standard_port() {
        let config = from_json(SAMPLE).expect("能解析");
        let (name, profile) = resolve(&config, None).expect("有默认");
        assert_eq!(name, "dev");
        assert_eq!(profile.port, 3306);
        assert_eq!(profile.database, "fm_dev");
    }

    #[test]
    fn an_explicit_profile_wins_and_keeps_its_password_variable() {
        let config = from_json(SAMPLE).expect("能解析");
        let (name, profile) = resolve(&config, Some("test")).expect("能选中");
        assert_eq!(name, "test");
        assert_eq!(profile.port, 3307);
        assert_eq!(profile.password_env.as_deref(), Some("MYSQL_PWD_FM_TEST"));
    }

    #[test]
    fn a_missing_default_or_unknown_profile_is_an_error() {
        let broken = r#"{ "default": "prod", "profiles": { "dev": { "host": "h", "user": "u", "database": "d" } } }"#;
        assert!(from_json(broken).is_err(), "default 指向不存在的 profile");
        let config = from_json(SAMPLE).expect("能解析");
        assert!(
            resolve(&config, Some("prod")).is_err(),
            "选不存在的 profile"
        );
    }

    #[test]
    fn a_password_in_the_file_is_refused() {
        let leaky = r#"{ "default": "dev", "profiles": { "dev": { "host": "h", "user": "u", "database": "d", "password": "x" } } }"#;
        let err = from_json(leaky).expect_err("必须拒绝");
        assert!(err.to_string().contains("password_env"), "{err}");
    }

    #[test]
    fn a_typo_in_a_field_name_is_refused() {
        let typo = r#"{ "default": "dev", "profiles": { "dev": { "host": "h", "user": "u", "databse": "d" } } }"#;
        assert!(from_json(typo).is_err(), "字段名拼错要报出来");
    }
}
