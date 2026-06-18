use mcp_universal::config::AppConfig;
use serial_test::serial;
use std::io::Write;
use tempfile::NamedTempFile;

fn clean_env() {
    std::env::remove_var("MCP_SERVER_HOST");
    std::env::remove_var("MCP_SERVER_PORT");
    std::env::remove_var("MCP_PUBLIC_URL");
    std::env::remove_var("MCP_AUTH_TOKEN");
    std::env::remove_var("GETAIBD_API_KEY");
    std::env::remove_var("GETAIBD_BASE_URL");
    std::env::remove_var("GETAIBD_DEFAULT_MODEL");
}

fn empty_config_file() -> NamedTempFile {
    NamedTempFile::new().unwrap()
}

#[test]
#[serial]
fn default_config_has_correct_server_settings() {
    clean_env();
    let file = empty_config_file();
    let config = AppConfig::load(Some(file.path()));
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 3333);
    assert_eq!(config.server.request_timeout_secs, 60);
    assert!(config.server.public_url.is_none());
}

#[test]
#[serial]
fn default_config_has_no_providers() {
    clean_env();
    let file = empty_config_file();
    let config = AppConfig::load(Some(file.path()));
    assert!(config.providers.openai_compat.is_empty());
}

#[test]
#[serial]
fn env_var_overrides_toml_server_port() {
    clean_env();
    std::env::set_var("MCP_SERVER_PORT", "9999");

    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r"
[server]
port = 3333
"
    )
    .unwrap();

    let config = AppConfig::load(Some(file.path()));
    assert_eq!(config.server.port, 9999);
    clean_env();
}

#[test]
#[serial]
fn env_var_overrides_toml_server_host() {
    clean_env();
    std::env::set_var("MCP_SERVER_HOST", "0.0.0.0");

    let file = empty_config_file();
    let config = AppConfig::load(Some(file.path()));
    assert_eq!(config.server.host, "0.0.0.0");
    clean_env();
}

#[test]
#[serial]
fn env_var_sets_public_url() {
    clean_env();
    std::env::set_var("MCP_PUBLIC_URL", "https://tunnel.example.com");

    let file = empty_config_file();
    let config = AppConfig::load(Some(file.path()));
    assert_eq!(
        config.server.public_url.as_deref(),
        Some("https://tunnel.example.com")
    );
    clean_env();
}

#[test]
#[serial]
fn auth_token_set_from_env_var() {
    clean_env();
    std::env::set_var("MCP_AUTH_TOKEN", "secret-token");

    let file = empty_config_file();
    let config = AppConfig::load(Some(file.path()));
    assert_eq!(config.server.auth_token.as_deref(), Some("secret-token"));
    clean_env();
}

#[test]
#[serial]
fn getaibd_provider_enabled_from_env_var() {
    clean_env();
    std::env::set_var("GETAIBD_API_KEY", "gk-test-key");

    let file = empty_config_file();
    let config = AppConfig::load(Some(file.path()));
    let provider = config
        .providers
        .openai_compat
        .iter()
        .find(|c| c.id == "getaibd")
        .expect("getaibd provider should be registered");
    assert!(provider.enabled);
    assert_eq!(provider.api_key.as_deref(), Some("gk-test-key"));
    assert_eq!(provider.base_url, "https://getaibd.com/v1/api");
    assert!(provider.supports_tool_calling);
    assert_eq!(config.memory.embedding_provider, "getaibd");
    clean_env();
}

#[test]
#[serial]
fn getaibd_base_url_overridable() {
    clean_env();
    std::env::set_var("GETAIBD_API_KEY", "gk-test-key");
    std::env::set_var("GETAIBD_BASE_URL", "http://localhost:8000/v1/api");

    let file = empty_config_file();
    let config = AppConfig::load(Some(file.path()));
    let provider = config
        .providers
        .openai_compat
        .iter()
        .find(|c| c.id == "getaibd")
        .unwrap();
    assert_eq!(provider.base_url, "http://localhost:8000/v1/api");
    clean_env();
}

#[test]
#[serial]
fn missing_config_file_uses_defaults() {
    clean_env();
    let config = AppConfig::load(Some(std::path::Path::new("/nonexistent/config.toml")));
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 3333);
}

#[test]
#[serial]
fn malformed_toml_uses_defaults() {
    clean_env();
    let mut file = NamedTempFile::new().unwrap();
    writeln!(file, "this is not valid toml {{{{").unwrap();

    let config = AppConfig::load(Some(file.path()));
    assert_eq!(config.server.host, "127.0.0.1");
    assert_eq!(config.server.port, 3333);
}
