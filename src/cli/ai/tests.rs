use zeroize::Zeroizing;

use super::{
    models::{MODEL_LIST_MAX_BYTES, list_models_live, parse_model_names},
    wizard::run_with_io,
    *,
};
use crate::config::{
    AiAuth, Config, ConfigPaths, ProviderCredential, read_credential, write_credential,
};

fn paths(directory: &tempfile::TempDir) -> ConfigPaths {
    let config = directory.path().join("config");
    ConfigPaths {
        config_file: config.join("config.toml"),
        credentials_file: config.join("credentials.toml"),
        specs_directory: config.join("specs"),
        state_directory: directory.path().join("state"),
        cache_directory: directory.path().join("cache"),
    }
}

fn fake_oauth_tokens() -> OAuthTokens {
    OAuthTokens {
        access_token: Zeroizing::new("fake-access-token".to_owned()),
        refresh_token: Zeroizing::new("fake-refresh-token".to_owned()),
        expires_at: 9_999_999_999,
        account_id: Some("acct-test-1".to_owned()),
    }
}

/// 默认假依赖：不触网、无环境变量；意外的 OAuth 调用直接 panic。
fn base_deps() -> Deps {
    Deps {
        device_flow: Box::new(|_, _| panic!("device flow not expected in this test")),
        gemini_flow: Box::new(|_, _| panic!("gemini flow not expected in this test")),
        list_models: Box::new(|_| None),
        connection_test: Box::new(|_, _| Ok(())),
        env_get: Box::new(|_| None),
    }
}

fn run_script(
    script: &str,
    paths: &ConfigPaths,
    deps: Deps,
) -> (String, String, crate::Result<()>) {
    let mut input = script.as_bytes();
    let mut output = Vec::new();
    let mut err = Vec::new();
    let result = run_with_io(&mut input, &mut output, &mut err, paths, deps);
    (
        String::from_utf8(output).expect("UTF-8 output"),
        String::from_utf8(err).expect("UTF-8 err"),
        result,
    )
}

fn provider_number(slug: &str) -> usize {
    crate::ai::providers::registry()
        .iter()
        .position(|spec| spec.slug == slug)
        .map(|index| index + 1)
        .expect("provider registered")
}

#[cfg(unix)]
fn credentials_mode(paths: &ConfigPaths) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(&paths.credentials_file)
        .expect("credentials metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn deepseek_happy_path_writes_config_and_private_credential() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.list_models = Box::new(|query| {
        assert_eq!(query.slug, "deepseek");
        assert_eq!(query.bearer, Some("test-key-12345"));
        Some(vec!["ds-live-1".to_owned(), "ds-live-2".to_owned()])
    });
    let (output, err, result) = run_script("1\ntest-key-12345\n\n", &paths, deps);
    result.expect("wizard should succeed");

    let config = Config::load(&paths.config_file).expect("load config");
    assert!(config.ai.enabled);
    assert_eq!(config.ai.provider, "deepseek");
    assert_eq!(config.ai.auth, AiAuth::ApiKey);
    assert_eq!(config.ai.endpoint, "https://api.deepseek.com/v1");
    assert_eq!(config.ai.model, "ds-live-1");
    assert_eq!(config.ai.account_id, None);
    assert_eq!(
        config.ai.api_key_file.as_deref(),
        Some(Path::new("credentials.toml"))
    );

    match read_credential(&paths.credentials_file, "deepseek").expect("read credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "test-key-12345"),
        ProviderCredential::OAuth(_) => panic!("deepseek must store an API key"),
    }
    let stored = std::fs::read_to_string(&paths.credentials_file).expect("credentials file");
    assert!(stored.contains("version = 2"));
    #[cfg(unix)]
    assert_eq!(credentials_mode(&paths), 0o600);

    assert!(output.contains("✓ 连接成功"));
    assert!(output.contains("??"));
    // secret 绝不出现在任何输出中。
    assert!(!output.contains("test-key-12345"));
    assert!(!err.contains("test-key-12345"));
}

#[test]
fn opencode_go_subscription_uses_its_zen_endpoint_and_key_store() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let script = format!(
        "{}\nopencode-go-key-123\n\n",
        provider_number("opencode-go")
    );
    let (output, err, result) = run_script(&script, &paths, base_deps());
    result.expect("OpenCode Go wizard should succeed");

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "opencode-go");
    assert_eq!(config.ai.endpoint, "https://opencode.ai/zen/go/v1");
    assert_eq!(config.ai.model, "kimi-k2.7-code");
    match read_credential(&paths.credentials_file, "opencode-go").expect("credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "opencode-go-key-123"),
        ProviderCredential::OAuth(_) => panic!("OpenCode Go uses an API key"),
    }
    assert!(output.contains("OpenCode Go"));
    assert!(!output.contains("opencode-go-key-123"));
    assert!(!err.contains("opencode-go-key-123"));
}

#[test]
fn openrouter_uses_the_openrouter_endpoint_and_key_store() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let script = format!("{}\nopenrouter-key-123\n\n", provider_number("openrouter"));
    let (_output, _err, result) = run_script(&script, &paths, base_deps());
    result.expect("OpenRouter wizard should succeed");

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "openrouter");
    assert_eq!(config.ai.endpoint, "https://openrouter.ai/api/v1");
    assert_eq!(config.ai.model, "anthropic/claude-sonnet-4.6");
    match read_credential(&paths.credentials_file, "openrouter").expect("credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "openrouter-key-123"),
        ProviderCredential::OAuth(_) => panic!("OpenRouter uses an API key"),
    }
}

#[test]
fn common_api_key_providers_use_registry_defaults_and_private_credentials() {
    for (slug, endpoint, model) in [
        (
            "anthropic",
            "https://api.anthropic.com/v1",
            "claude-sonnet-4-6",
        ),
        (
            "groq",
            "https://api.groq.com/openai/v1",
            "llama-3.3-70b-versatile",
        ),
        (
            "alibaba-coding-plan",
            "https://coding-intl.dashscope.aliyuncs.com/v1",
            "qwen3.7-plus",
        ),
        ("opencode-zen", "https://opencode.ai/zen/v1", "gpt-5.4"),
        ("kimi-coding", "https://api.kimi.com/coding", "kimi-k3"),
        (
            "novita",
            "https://api.novita.ai/openai/v1",
            "moonshotai/kimi-k2.5",
        ),
    ] {
        let directory = tempfile::tempdir().expect("directory");
        let paths = paths(&directory);
        let key = format!("{slug}-test-key");
        let script = format!("{}\n{key}\n\n", provider_number(slug));
        let (output, err, result) = run_script(&script, &paths, base_deps());
        result.unwrap_or_else(|error| panic!("{slug} wizard failed: {error}"));

        let config = Config::load(&paths.config_file).expect("load config");
        assert_eq!(config.ai.provider, slug);
        assert_eq!(config.ai.endpoint, endpoint);
        assert_eq!(config.ai.model, model);
        match read_credential(&paths.credentials_file, slug).expect("credential") {
            ProviderCredential::ApiKey(stored) => assert_eq!(stored.as_str(), key),
            ProviderCredential::OAuth(_) => panic!("{slug} uses an API key"),
        }
        assert!(!output.contains(&key));
        assert!(!err.contains(&key));
        #[cfg(unix)]
        assert_eq!(credentials_mode(&paths), 0o600);
    }
}

#[test]
fn provider_environment_alias_is_detected() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.env_get =
        Box::new(|name| (name == "GLM_API_KEY").then(|| "glm-environment-key".to_owned()));
    let script = format!("{}\n\n\n", provider_number("zai"));
    let (output, err, result) = run_script(&script, &paths, deps);
    result.expect("Z.AI wizard should succeed");

    assert!(output.contains("$GLM_API_KEY"));
    assert!(!output.contains("glm-environment-key"));
    assert!(!err.contains("glm-environment-key"));
    match read_credential(&paths.credentials_file, "zai").expect("credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "glm-environment-key"),
        ProviderCredential::OAuth(_) => panic!("Z.AI uses an API key"),
    }
}

#[test]
fn current_provider_is_marked_and_used_as_default() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut config = Config::default();
    config.ai.enabled = true;
    config.ai.provider = "deepseek".into();
    config.ai.endpoint = "https://api.deepseek.com/v1".into();
    config.ai.model = "deepseek-reasoner".into();
    config.ai.api_key_env = String::new();
    config.ai.api_key_file = Some(PathBuf::from("credentials.toml"));
    config
        .write_atomic(&paths.config_file)
        .expect("write config");

    let mut deps = base_deps();
    deps.list_models = Box::new(|_| Some(vec!["ds-live-1".to_owned()]));
    // Enter 接受服务商默认值 deepseek，随后输入密钥，Enter 接受当前模型。
    let (output, _err, result) = run_script("\nre-key-999\n\n", &paths, deps);
    result.expect("wizard should succeed");
    assert!(output.contains("当前"));

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "deepseek");
    // 重新配置同一服务商时，默认模型是现有配置里的模型。
    assert_eq!(config.ai.model, "deepseek-reasoner");
    match read_credential(&paths.credentials_file, "deepseek").expect("read credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "re-key-999"),
        ProviderCredential::OAuth(_) => panic!("deepseek must store an API key"),
    }
}

#[test]
fn ollama_skips_credentials_and_uses_live_models() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.list_models = Box::new(|query| {
        assert_eq!(query.slug, "ollama");
        assert_eq!(query.bearer, None);
        Some(vec!["llama3.2".to_owned()])
    });
    let (output, _err, result) = run_script("7\n\n", &paths, deps);
    result.expect("wizard should succeed");

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "ollama");
    assert_eq!(config.ai.endpoint, "http://localhost:11434/v1");
    assert_eq!(config.ai.model, "llama3.2");
    assert!(config.ai.api_key_env.is_empty());
    assert!(config.ai.api_key_file.is_none());
    assert!(
        !paths.credentials_file.exists(),
        "ollama must not create a credential file"
    );
    assert!(!output.contains("API Key: "));
    assert!(output.contains("[3/6] 配置凭据"));
    assert!(output.contains("无需配置凭据"));
}

#[test]
fn lmstudio_skips_credentials_and_uses_live_models() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.list_models = Box::new(|query| {
        assert_eq!(query.slug, "lmstudio");
        assert_eq!(query.bearer, None);
        Some(vec!["local-model".to_owned()])
    });
    let script = format!("{}\n\n", provider_number("lmstudio"));
    let (output, _err, result) = run_script(&script, &paths, deps);
    result.expect("wizard should succeed");

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "lmstudio");
    assert_eq!(config.ai.endpoint, "http://127.0.0.1:1234/v1");
    assert_eq!(config.ai.model, "local-model");
    assert!(config.ai.api_key_env.is_empty());
    assert!(config.ai.api_key_file.is_none());
    assert!(!paths.credentials_file.exists());
    assert!(output.contains("[3/6] 配置凭据"));
    assert!(output.contains("无需配置凭据"));
}

#[test]
fn empty_model_list_requires_manual_input() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    // ollama 在线拉取成功但没有已安装的模型。
    deps.list_models = Box::new(|_| Some(Vec::new()));
    let (output, _err, result) = run_script("7\nmy-local-model\n", &paths, deps);
    result.expect("wizard should succeed");
    assert!(output.contains("未能获取模型列表"));
    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.model, "my-local-model");
}

#[test]
fn gemini_api_key_auth_is_recorded() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    // 拉取失败 → 回退静态表 gemini-2.5-pro。
    let (_output, _err, result) = run_script("4\ngemini-key-xyz\n\n", &paths, base_deps());
    result.expect("wizard should succeed");

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "gemini");
    assert_eq!(config.ai.auth, AiAuth::ApiKey);
    assert_eq!(config.ai.model, "gemini-2.5-pro");
    match read_credential(&paths.credentials_file, "gemini").expect("read credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "gemini-key-xyz"),
        ProviderCredential::OAuth(_) => panic!("gemini must store an API key"),
    }
}

#[test]
fn grok_oauth_flow_writes_tokens_and_account_id() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.device_flow = Box::new(|slug, sink| {
        assert_eq!(slug, "grok-oauth");
        sink(DevicePrompt {
            verification_uri: "https://x.ai/device".to_owned(),
            user_code: "ABCD-1234".to_owned(),
        });
        Ok(fake_oauth_tokens())
    });
    let (output, err, result) = run_script("5\n\n", &paths, deps);
    result.expect("wizard should succeed");
    assert!(output.contains("请在浏览器打开: https://x.ai/device"));
    assert!(output.contains("并输入代码: ABCD-1234"));

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "grok-oauth");
    assert_eq!(config.ai.auth, AiAuth::OAuth);
    assert_eq!(config.ai.model, "grok-4.5");
    assert_eq!(config.ai.account_id.as_deref(), Some("acct-test-1"));

    match read_credential(&paths.credentials_file, "grok-oauth").expect("read credential") {
        ProviderCredential::OAuth(tokens) => {
            assert_eq!(tokens.access_token.as_str(), "fake-access-token");
            assert_eq!(tokens.refresh_token.as_str(), "fake-refresh-token");
            assert_eq!(tokens.account_id.as_deref(), Some("acct-test-1"));
        }
        ProviderCredential::ApiKey(_) => panic!("grok-oauth must store OAuth tokens"),
    }
    #[cfg(unix)]
    assert_eq!(credentials_mode(&paths), 0o600);
    assert!(!output.contains("fake-access-token"));
    assert!(!err.contains("fake-access-token"));
}

#[test]
fn gemini_oauth_eof_cancels_without_writing() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.gemini_flow = Box::new(|sink, read_code| {
        sink("https://accounts.google.com/o/oauth2/v2/auth?fake=1".to_owned());
        match read_code() {
            Err(OAuthError::Cancelled) => Err(OAuthError::Cancelled),
            other => panic!("EOF must map to Cancelled, got {other:?}"),
        }
    });
    // 输入在授权代码提示处直接 EOF。
    let (output, _err, result) = run_script("3\n", &paths, deps);
    result.expect("cancellation is not an error");
    assert!(output.contains("授权代码"));
    assert!(output.contains("已取消"));
    assert!(!paths.config_file.exists());
    assert!(!paths.credentials_file.exists());
}

#[test]
fn custom_endpoint_reprompts_until_valid() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    // custom 无静态模型表，拉取失败 → 手动输入模型。
    let script = format!(
        "{}\nbad url with spaces\napi.example.com/v1\ncustom-key-1\nmy-model\n",
        provider_number("custom")
    );
    let (output, err, result) = run_script(&script, &paths, base_deps());
    result.expect("wizard should succeed");
    assert!(err.contains("端点"));

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "custom");
    // 缺少 scheme 时自动补 https://。
    assert_eq!(config.ai.endpoint, "https://api.example.com/v1");
    assert_eq!(config.ai.model, "my-model");
    match read_credential(&paths.credentials_file, "custom").expect("read credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "custom-key-1"),
        ProviderCredential::OAuth(_) => panic!("custom must store an API key"),
    }
    assert!(!output.contains("custom-key-1"));
}

#[test]
fn quit_at_first_prompt_writes_nothing() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let (output, _err, result) = run_script("q\n", &paths, base_deps());
    result.expect("quit is not an error");
    assert!(output.contains("已取消"));
    assert!(!paths.config_file.exists());
    assert!(!paths.credentials_file.exists());
}

#[test]
fn quit_mid_flow_writes_nothing() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    // 在模型菜单退出：凭据尚未写入磁盘。
    let (output, _err, result) = run_script("1\nsome-key-abc\nq\n", &paths, base_deps());
    result.expect("quit is not an error");
    assert!(output.contains("已取消"));
    assert!(!paths.config_file.exists());
    assert!(!paths.credentials_file.exists());
}

#[test]
fn invalid_secret_reprompts_then_aborts() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    // 三次无效密钥（首尾空白违反 validate_secret 规则）后向导取消。
    let (_output, err, result) = run_script("1\n spaced-key \n\t\n  \n", &paths, base_deps());
    result.expect("abort is not an error");
    assert!(err.contains("密钥无效"));
    assert!(!paths.config_file.exists());
    assert!(!paths.credentials_file.exists());
}

#[test]
fn connection_failure_save_anyway_persists() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.connection_test = Box::new(|_, _| Err("HK-AI-NET AI network request failed".to_owned()));
    // r 重试一次后仍然失败，最后 s 保存。
    let (output, err, result) = run_script("1\nretry-key-1\n\nr\ns\n", &paths, deps);
    result.expect("wizard should succeed");
    assert!(err.contains("✗ HK-AI-NET"));

    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "deepseek");
    match read_credential(&paths.credentials_file, "deepseek").expect("read credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "retry-key-1"),
        ProviderCredential::OAuth(_) => panic!("deepseek must store an API key"),
    }
    assert!(!output.contains("retry-key-1"));
}

#[test]
fn connection_failure_abort_removes_new_credential() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.connection_test = Box::new(|_, _| Err("HK-AI-NET AI network request failed".to_owned()));
    let (output, _err, result) = run_script("1\nabort-key-1\n\nq\n", &paths, deps);
    result.expect("abort is not an error");
    assert!(output.contains("已放弃"));
    assert!(!paths.config_file.exists(), "config must not be written");
    assert!(
        !paths.credentials_file.exists(),
        "pre-written credential must be removed on abort"
    );
}

#[test]
fn connection_failure_abort_restores_previous_credential() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    write_credential(
        &paths.credentials_file,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("old-key-1".to_owned())),
    )
    .expect("write previous credential");
    let mut deps = base_deps();
    deps.connection_test = Box::new(|_, _| Err("HK-AI-NET AI network request failed".to_owned()));
    let (_output, _err, result) = run_script("1\nnew-key-1\n\nq\n", &paths, deps);
    result.expect("abort is not an error");
    assert!(!paths.config_file.exists(), "config must not be written");
    match read_credential(&paths.credentials_file, "deepseek").expect("read credential") {
        ProviderCredential::ApiKey(key) => {
            assert_eq!(key.as_str(), "old-key-1", "previous credential restored")
        }
        ProviderCredential::OAuth(_) => panic!("deepseek must store an API key"),
    }
}

#[test]
fn env_key_is_offered_and_used_as_default() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.env_get =
        Box::new(|name| (name == "DEEPSEEK_API_KEY").then(|| "env-secret-777".to_owned()));
    // Enter 接受"使用环境变量密钥"。
    let (output, err, result) = run_script("1\n\n\n", &paths, deps);
    result.expect("wizard should succeed");
    assert!(output.contains("$DEEPSEEK_API_KEY"));
    match read_credential(&paths.credentials_file, "deepseek").expect("read credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "env-secret-777"),
        ProviderCredential::OAuth(_) => panic!("deepseek must store an API key"),
    }
    assert!(!output.contains("env-secret-777"));
    assert!(!err.contains("env-secret-777"));
}

#[test]
fn env_key_can_be_declined_for_manual_entry() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    deps.env_get =
        Box::new(|name| (name == "DEEPSEEK_API_KEY").then(|| "env-secret-777".to_owned()));
    let (_output, _err, result) = run_script("1\nn\ntyped-key-1\n\n", &paths, deps);
    result.expect("wizard should succeed");
    match read_credential(&paths.credentials_file, "deepseek").expect("read credential") {
        ProviderCredential::ApiKey(key) => assert_eq!(key.as_str(), "typed-key-1"),
        ProviderCredential::OAuth(_) => panic!("deepseek must store an API key"),
    }
}

#[test]
fn broken_existing_config_aborts_before_any_write() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    std::fs::create_dir_all(paths.config_file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&paths.config_file, "version = 1\nunknown = true\n").expect("write config");
    let (_output, _err, result) = run_script("1\nkey\n", &paths, base_deps());
    result.expect_err("parse error must abort the wizard");
    assert!(!paths.credentials_file.exists());
}

#[test]
fn non_terminal_stdio_is_rejected_before_any_io() {
    // cargo test 会捕获 stdout（非终端），门控必然触发。
    let error = run(AiCommand::Setup).expect_err("non-terminal stdio must be rejected");
    assert!(error.to_string().contains("hokan config ai"));
}

#[test]
fn parse_model_names_accepts_openai_ollama_and_codex_shapes() {
    let openai = parse_model_names(br#"{"data":[{"id":"m1"},{"id":"m2"}]}"#).expect("openai");
    assert_eq!(openai, ["m1", "m2"]);
    let ollama = parse_model_names(br#"{"models":[{"name":"llama3.2"}]}"#).expect("ollama");
    assert_eq!(ollama, ["llama3.2"]);
    let codex = parse_model_names(br#"{"models":[{"slug":"gpt-5.5"}]}"#).expect("codex");
    assert_eq!(codex, ["gpt-5.5"]);
    assert!(parse_model_names(br#"{"error":"boom"}"#).is_none());
    assert!(parse_model_names(b"not json").is_none());
}

/// 写入内容包含 marker 时注入 I/O 错误的输出 writer（模拟终端写入失败）。
struct MarkerFailingWriter {
    inner: Vec<u8>,
    marker: &'static str,
}

impl std::io::Write for MarkerFailingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if std::str::from_utf8(buffer).is_ok_and(|text| text.contains(self.marker)) {
            return Err(std::io::Error::other("injected output failure"));
        }
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[test]
fn io_error_after_credential_write_restores_previous_credential() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    write_credential(
        &paths.credentials_file,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("old-key-1".to_owned())),
    )
    .expect("write previous credential");
    // 输出在"正在测试连接…"（凭据已写入之后）处注入 I/O 错误。
    let mut input = "1\nnew-key-1\n\n".as_bytes();
    let mut output = MarkerFailingWriter {
        inner: Vec::new(),
        marker: "正在测试连接",
    };
    let mut err = Vec::new();
    let result = run_with_io(&mut input, &mut output, &mut err, &paths, base_deps());
    result.expect_err("output failure must abort the wizard");
    assert!(!paths.config_file.exists(), "config must not be written");
    match read_credential(&paths.credentials_file, "deepseek").expect("read credential") {
        ProviderCredential::ApiKey(key) => {
            assert_eq!(key.as_str(), "old-key-1", "previous credential restored")
        }
        ProviderCredential::OAuth(_) => panic!("deepseek must store an API key"),
    }
}

#[cfg(unix)]
#[test]
fn config_write_failure_restores_previous_credential() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("directory");
    let mut paths = paths(&directory);
    // config.toml 放进只读目录让 write_atomic 必然失败；credentials.toml 留在
    // 可写目录，恢复必须能成功写回。
    let readonly = directory.path().join("readonly");
    std::fs::create_dir_all(&readonly).expect("mkdir");
    paths.config_file = readonly.join("config.toml");
    write_credential(
        &paths.credentials_file,
        "deepseek",
        &ProviderCredential::ApiKey(Zeroizing::new("old-key-1".to_owned())),
    )
    .expect("write previous credential");
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555))
        .expect("chmod readonly");

    let (_output, _err, result) = run_script("1\nnew-key-1\n\n", &paths, base_deps());
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o755))
        .expect("restore permissions for cleanup");
    result.expect_err("config write failure must abort the wizard");
    match read_credential(&paths.credentials_file, "deepseek").expect("read credential") {
        ProviderCredential::ApiKey(key) => {
            assert_eq!(key.as_str(), "old-key-1", "previous credential restored")
        }
        ProviderCredential::OAuth(_) => panic!("deepseek must store an API key"),
    }
}

#[test]
fn corrupt_existing_credential_entry_aborts_before_write() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    // 条目同时带 api_key 与 OAuth 字段：存储层解析正常，条目级读取报
    // InvalidFormat；向导必须中止而不是当作"没有旧凭据"继续覆盖。
    let corrupt = "version = 2\n\
                   [providers.deepseek]\n\
                   api_key = \"old-key-1\"\n\
                   access_token = \"token\"\n\
                   refresh_token = \"token-2\"\n\
                   expires_at = 1\n";
    std::fs::create_dir_all(paths.credentials_file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&paths.credentials_file, corrupt).expect("write corrupt file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &paths.credentials_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("permissions");
    }

    let (_output, _err, result) = run_script("1\nnew-key-1\n\n", &paths, base_deps());
    let error = result.expect_err("corrupt credential entry must abort the wizard");
    assert!(error.to_string().contains("损坏"));
    assert!(!paths.config_file.exists(), "config must not be written");
    assert_eq!(
        std::fs::read_to_string(&paths.credentials_file).expect("read file"),
        corrupt,
        "corrupt file must be left untouched"
    );
}

#[test]
fn abort_after_v1_migration_restores_file_bytes() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    // v1 旧文件：首次 v2 写入会把旧 key 迁移到保留 slug 下；放弃时必须
    // 字节级还原，而不是留下"新 slug → 旧 key"的多余条目。
    let v1 = "version = 1\napi_key = \"legacy-key-1\"\n";
    std::fs::create_dir_all(paths.credentials_file.parent().expect("parent")).expect("mkdir");
    std::fs::write(&paths.credentials_file, v1).expect("write v1 file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &paths.credentials_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("permissions");
    }
    let mut deps = base_deps();
    deps.connection_test = Box::new(|_, _| Err("HK-AI-NET AI network request failed".to_owned()));
    let (output, _err, result) = run_script("1\nnew-key-1\n\nq\n", &paths, deps);
    result.expect("abort is not an error");
    assert!(output.contains("已放弃"));
    assert!(!paths.config_file.exists(), "config must not be written");
    assert_eq!(
        std::fs::read_to_string(&paths.credentials_file).expect("read file"),
        v1,
        "v1 file must be restored byte-for-byte"
    );
}

/// 起一次性的本地 HTTP 服务器返回给定响应体，返回模型列表端点。
fn spawn_model_server(body: Vec<u8>) -> String {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(header.as_bytes()).expect("write header");
        stream.write_all(&body).expect("write body");
    });
    format!("http://127.0.0.1:{port}/v1")
}

#[test]
fn list_models_live_accepts_small_body() {
    let endpoint = spawn_model_server(br#"{"data":[{"id":"m1"},{"id":"m2"}]}"#.to_vec());
    let query = ModelListQuery {
        slug: "deepseek",
        endpoint: &endpoint,
        bearer: None,
        account_id: None,
    };
    assert_eq!(
        list_models_live(&query),
        Some(vec!["m1".to_owned(), "m2".to_owned()])
    );
}

#[test]
fn list_models_live_rejects_oversized_body() {
    // 合法 JSON + 尾部空白填充到超限：无上限时会被成功解析。
    let mut body = br#"{"data":[{"id":"m1"}]}"#.to_vec();
    body.resize(MODEL_LIST_MAX_BYTES + 1, b' ');
    let endpoint = spawn_model_server(body);
    let query = ModelListQuery {
        slug: "deepseek",
        endpoint: &endpoint,
        bearer: None,
        account_id: None,
    };
    assert!(list_models_live(&query).is_none());
}

#[test]
fn remote_model_names_with_control_chars_are_dropped() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    let mut deps = base_deps();
    // 夹带 ANSI escape 的模型名放在第一位：不过滤会成为默认模型并被打印。
    deps.list_models = Box::new(|_| {
        Some(vec![
            "evil\u{1b}[31m-model".to_owned(),
            "good-model".to_owned(),
        ])
    });
    let (output, _err, result) = run_script("1\nctl-key-1\n\n", &paths, deps);
    result.expect("wizard should succeed");
    assert!(
        !output.contains('\u{1b}'),
        "escape sequences must not be printed"
    );
    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.model, "good-model");
}

#[test]
fn manual_model_name_with_control_chars_is_rejected() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    // 模型菜单选 m 手动输入：先输入夹带控制字符的名字（被拒绝），再输入合法名字。
    let (output, err, result) = run_script(
        "1\nctl-key-2\nm\nbad\u{1b}name\nclean-name\n",
        &paths,
        base_deps(),
    );
    result.expect("wizard should succeed");
    assert!(err.contains("控制字符"));
    assert!(
        !output.contains('\u{1b}'),
        "escape sequences must not be printed"
    );
    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.model, "clean-name");
}

/// 预置一份 API-key 服务商的旧配置（带 api_key_file）。
fn seed_api_key_config(paths: &ConfigPaths) {
    let mut config = Config::default();
    config.ai.enabled = true;
    config.ai.provider = "deepseek".into();
    config.ai.auth = AiAuth::ApiKey;
    config.ai.endpoint = "https://api.deepseek.com/v1".into();
    config.ai.model = "deepseek-chat".into();
    config.ai.api_key_env.clear();
    config.ai.api_key_file = Some(PathBuf::from("credentials.toml"));
    config
        .write_atomic(&paths.config_file)
        .expect("write config");
}

#[test]
fn switching_to_ollama_clears_stale_api_key_file() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    seed_api_key_config(&paths);
    let mut deps = base_deps();
    deps.list_models = Box::new(|_| Some(vec!["llama3.2".to_owned()]));
    let (_output, _err, result) = run_script("7\n\n", &paths, deps);
    result.expect("wizard should succeed");
    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "ollama");
    assert!(
        config.ai.api_key_file.is_none(),
        "stale api_key_file must be cleared"
    );
    assert!(
        config.ai.api_key_env.is_empty(),
        "credential-free providers must not retain an environment source"
    );
}

#[test]
fn switching_to_oauth_clears_stale_api_key_file() {
    let directory = tempfile::tempdir().expect("directory");
    let paths = paths(&directory);
    seed_api_key_config(&paths);
    let mut deps = base_deps();
    deps.device_flow = Box::new(|slug, _sink| {
        assert_eq!(slug, "grok-oauth");
        Ok(fake_oauth_tokens())
    });
    let (_output, _err, result) = run_script("5\n\n", &paths, deps);
    result.expect("wizard should succeed");
    let config = Config::load(&paths.config_file).expect("load config");
    assert_eq!(config.ai.provider, "grok-oauth");
    assert_eq!(config.ai.auth, AiAuth::OAuth);
    assert!(
        config.ai.api_key_file.is_none(),
        "stale api_key_file must be cleared"
    );
}
