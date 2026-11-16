//! 启动配置契约：范围、未知字段和凭据脱敏。
//! Startup configuration contracts: ranges, unknown fields, and credential redaction.

use ling_server::Config;

/// 安装包默认配置遵循 Fast / Journal 互斥契约，避免服务安装后立即退出。
/// Packaged defaults honor the Fast/Journal exclusion so installed services do not immediately exit.
#[test]
fn packaged_defaults_are_valid_fast_configurations() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for path in [
        "packaging/ling.toml",
        "packaging/linux/ling.toml",
        "packaging/macos/ling.toml",
    ] {
        let config =
            Config::load(&root.join(path)).unwrap_or_else(|error| panic!("{path}: {error}"));
        assert_eq!(config.events.mode, "fast");
        assert!(config.event_journal.dir.as_os_str().is_empty());
    }
}

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
        "[events]\nunknown=true",
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

/// 固定分片和业务限额必须在监听前校验；Validate fixed sharding and business limits before listening.
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

/// 毫秒精度固定为一，维护预算必须为正，未知 TTL 字段不得被忽略。
/// Millisecond precision is fixed at one, maintenance budgets must be positive, and unknown TTL fields fail.
#[test]
fn ttl_settings_enforce_precision_and_bounded_rounds() {
    let defaults = Config::default();
    assert_eq!(defaults.ttl.precision_ms, 1);
    assert_eq!(defaults.ttl.maintenance_budget, 1024);
    let mut config: Config = toml::from_str("[ttl]\nprecision_ms=1\nmaintenance_budget=7").unwrap();
    config.validate().unwrap();
    config.ttl.precision_ms = 2;
    assert!(config.validate().is_err());
    config.ttl.precision_ms = 1;
    config.ttl.maintenance_budget = 0;
    assert!(config.validate().is_err());
    assert!(toml::from_str::<Config>("[ttl]\ninterval_ms=1").is_err());
}

/// 事件配置组合和相对 Journal 目录在监听前确定。
/// Event configuration combinations and relative Journal directories are resolved before listening.
#[test]
fn event_configuration_and_relative_directory() {
    let mut c = Config::default();
    c.events.mode = "reliable".into();
    assert!(c.validate().is_err());
    c.event_journal.dir = "journal".into();
    if cfg!(windows) {
        let error = c.validate().unwrap_err();
        assert_eq!(error.code, "ERR_CONFIG_INVALID");
        assert!(error.message.contains("not supported on Windows"));
    } else {
        c.validate().unwrap();
    }
    c.events.enabled = false;
    assert!(c.validate().is_err());
    c.events.enabled = true;
    c.events.retry_interval_ms = 0;
    assert!(c.validate().is_err());
    c.events.retry_interval_ms = 1;
    c.events.retention_days = u64::MAX;
    assert!(c.validate().is_err());
    c.events.retention_days = 7;
    c.event_journal.group_commit_ms = 1001;
    assert!(c.validate().is_err());
    let dir = std::env::temp_dir().join(format!("linger-path-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("server.toml");
    std::fs::write(
        &path,
        "[events]\nmode=\"reliable\"\n[event_journal]\ndir=\"journal\"\n",
    )
    .unwrap();
    if cfg!(windows) {
        let error = Config::load(&path).err().unwrap();
        assert_eq!(error.code, "ERR_CONFIG_INVALID");
        assert!(error.message.contains("not supported on Windows"));
        assert!(!dir.join("journal").exists());
    } else {
        assert_eq!(
            Config::load(&path).unwrap().event_journal.dir,
            dir.join("journal")
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}
