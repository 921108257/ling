use linger_server::Config;

#[test]
fn defaults_and_namespace_edges() {
    Config::default().validate().unwrap();
    let mut config = Config::default();
    config.database.databases = 65536;
    config.database.default_db = u16::MAX;
    config.validate().unwrap();
    config.database.databases = 65535;
    assert!(config.validate().is_err());
    config.database.databases = 0;
    assert!(config.validate().is_err());
}

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
        "[engine]\nshard_count=1",
        "[server]\nprot=6380",
        "[auth]\npassword=123",
        "[database]\ndatabases=1\ndatabases=2",
    ] {
        assert!(toml::from_str::<Config>(text).is_err());
    }
}

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
