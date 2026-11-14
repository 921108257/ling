//! 启动配置契约：范围、未知字段和凭据脱敏。
//! Startup configuration contracts: ranges, unknown fields, and credential redaction.

use linger_server::Config;

/// 默认值与 u16 数据库编号边界均经验证。
/// Validates defaults and u16 database namespace boundaries.
#[test]
fn defaults_and_namespace_edges() {
    Config::default().validate().unwrap();
    let mut config = Config::default();
    config.engine.shard_count = 1;
    config.database.databases = 65536;
    config.database.default_db = u16::MAX;
    config.validate().unwrap();
    config.database.databases = 65535;
    assert!(config.validate().is_err());
    config.database.databases = 0;
    assert!(config.validate().is_err());
}

/// 拒绝无效认证组合、网络预算和不支持的配置。
/// Rejects invalid authentication combinations, network budgets, and unsupported settings.
#[test]
fn rejects_invalid_auth_limits_and_unsupported_fields() {
    let mut config = Config::default();
    config.auth.password = "secret".into();
    assert!(config.validate().is_err());
    config.auth.enabled = true;
    config.validate().unwrap();
    config.auth.password.clear();
    assert!(config.validate().is_err());
    let mut config = Config::default();
    config.protocol.max_frame_bytes = 1024;
    assert!(config.validate().is_err());
    let mut config = Config::default();
    config.server.max_connections = 0;
    assert!(config.validate().is_err());
    for text in [
        "[ttl]\nprecision_ms=1",
        "[server]\nprot=6380",
        "[auth]\npassword=123",
        "[database]\ndatabases=1\ndatabases=2",
    ] {
        assert!(toml::from_str::<Config>(text).is_err());
    }
}

/// 配置解析错误不得包含源文件中的密码。
/// Configuration parse errors never include passwords from the source file.
#[test]
fn file_errors_never_expose_password_source() {
    let dir = std::env::temp_dir().join(format!("linger-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.toml");
    std::fs::write(&path, "[auth]\npassword=\"sensitive-password\" trailing").unwrap();
    let error = Config::load(&path).err().unwrap();
    assert!(!error.to_string().contains("sensitive-password"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// 单分片和业务限额必须在监听前校验；Validate shard count and business limits before listening.
#[test]
fn engine_limits_are_strict_and_available_to_protocol() {
    let mut config: Config = toml::from_str(
        "[engine]\nshard_count=1\nmax_value_bytes=10\nmax_key_bytes=32\nmax_nest_depth=2",
    )
    .unwrap();
    config.validate().unwrap();
    assert_eq!(config.limits().value.max_bytes, 10);
    assert_eq!(config.limits().value.max_key_bytes, 32);
    config.engine.shard_count = 2;
    config.validate().unwrap();
    config.engine.shard_count = u32::MAX;
    assert!(config.validate().is_err());
    config.engine.shard_count = 0;
    config.engine.max_nest_depth = 0;
    assert!(config.validate().is_err());
}

/// 所有淘汰策略可配置，拼写错误与无界采样在启动前拒绝。
/// Every eviction policy is configurable; typos and unbounded sampling fail before startup.
#[test]
fn eviction_settings_are_strict() {
    for policy in ["noeviction", "lru", "lfu", "random", "ttl-first"] {
        let source =
            format!("[engine]\nshard_count=4\n[eviction]\npolicy=\"{policy}\"\nsample_size=7");
        let mut config: Config = toml::from_str(&source).unwrap();
        config.validate().unwrap();
        assert_eq!(config.engine_config().unwrap().eviction.as_str(), policy);
        config.eviction.sample_size = 0;
        assert!(config.validate().is_err());
    }
    let mut config = Config::default();
    config.eviction.policy = "LRU".into();
    assert!(config.validate().is_err());
    config.eviction.policy = "lru".into();
    config.eviction.sample_size = usize::MAX;
    assert!(config.validate().is_err());
}
