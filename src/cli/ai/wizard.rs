//! 向导的步骤流（服务商 → 认证 → 凭据 → 模型 → 测试 → 写回）与提示辅助。

use std::{
    io::{BufRead, Write},
    path::PathBuf,
};

use zeroize::Zeroizing;

use super::{Deps, ModelListQuery};
use crate::{
    ai::{
        DevicePrompt, OAuthError,
        providers::{self, ProviderSpec},
    },
    config::{
        AiAuth, AiConfig, Config, ConfigPaths, CredentialError, OAuthTokens, ProviderCredential,
        delete_credential, read_credential, validate_secret, write_credential,
    },
};

/// 密钥输入的最大尝试次数；超出即取消向导（不写入任何内容）。
const MAX_SECRET_ATTEMPTS: usize = 3;

/// 凭据文件字节快照的大小上限，与 `credentials` 模块的读取上限一致。
const SNAPSHOT_MAX_BYTES: u64 = 16 * 1024;

pub(super) fn run_with_io(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    err: &mut dyn Write,
    paths: &ConfigPaths,
    deps: Deps,
) -> crate::Result<()> {
    // 先加载配置：解析失败立即中止，避免覆盖损坏的配置文件。
    let mut config = Config::load(&paths.config_file)?;
    let current_provider = (config.ai.enabled && !config.ai.provider.trim().is_empty())
        .then(|| config.ai.provider.trim().to_owned());
    let current_model = config.ai.model.trim().to_owned();

    let mut wizard = Wizard {
        input,
        output,
        err,
        deps,
    };
    writeln!(wizard.output, "\nhokan AI 配置向导（随时输入 q 退出）")?;

    // 第一步：服务商。
    let Some(spec) = wizard.choose_provider(current_provider.as_deref())? else {
        return wizard.cancelled();
    };
    // 第二步：认证方式（当前注册表每个条目只有单一方式，菜单预留给双方式条目）。
    let Some(auth) = wizard.choose_auth(spec)? else {
        return wizard.cancelled();
    };
    // `custom` 额外询问端点；其余服务商用注册表默认端点。
    let endpoint = if spec.slug == "custom" {
        let Some(endpoint) = wizard.prompt_endpoint()? else {
            return wizard.cancelled();
        };
        endpoint
    } else {
        spec.default_endpoint.to_owned()
    };

    // 第三步：凭据（ollama 无需凭据）。
    let credential: Option<ProviderCredential> = if spec.auth_methods.is_empty() {
        None
    } else {
        match auth {
            AiAuth::ApiKey => {
                let Some(key) = wizard.collect_api_key(spec)? else {
                    return wizard.cancelled();
                };
                Some(ProviderCredential::ApiKey(key))
            }
            AiAuth::OAuth => {
                let Some(tokens) = wizard.collect_oauth(spec)? else {
                    return wizard.cancelled();
                };
                Some(ProviderCredential::OAuth(tokens))
            }
        }
    };

    // 第四步：模型（先在线拉取，失败回退静态表）。
    let (bearer, account_id) = match &credential {
        Some(ProviderCredential::ApiKey(key)) => (Some(key.as_str().to_owned()), None),
        Some(ProviderCredential::OAuth(tokens)) => (
            Some(tokens.access_token.as_str().to_owned()),
            tokens.account_id.clone(),
        ),
        None => (None, None),
    };
    let query = ModelListQuery {
        slug: spec.slug,
        endpoint: &endpoint,
        bearer: bearer.as_deref(),
        account_id: account_id.as_deref(),
    };
    let same_provider = current_provider.as_deref() == Some(spec.slug);
    let current_model = (same_provider && !current_model.is_empty()).then_some(current_model);
    let Some(model) = wizard.choose_model(spec, &query, current_model.as_deref())? else {
        return wizard.cancelled();
    };

    // 待定配置：基于现有 [ai] 节克隆修改，保留 trigger_prefix 等其余字段。
    let mut ai: AiConfig = config.ai.clone();
    ai.enabled = true;
    ai.provider = spec.slug.to_owned();
    ai.auth = auth;
    ai.endpoint = endpoint;
    ai.model = model;
    ai.account_id = match &credential {
        Some(ProviderCredential::OAuth(tokens)) => tokens.account_id.clone(),
        _ => None,
    };
    ai.api_key_file = match &credential {
        Some(ProviderCredential::ApiKey(_)) => Some(PathBuf::from("credentials.toml")),
        // OAuth / 无凭据（ollama）：清掉旧配置里可能残留的 api_key_file。
        _ => None,
    };

    // 第五步：连接测试。AiClient 只能从磁盘读凭据，因此先写入；用户在失败
    // 菜单选择"放弃"时恢复写入前的凭据状态。
    let written = credential.is_some();
    let mut snapshot: Option<Zeroizing<Vec<u8>>> = None;
    if let Some(new_credential) = &credential {
        // 条目级错误（InvalidFormat/InvalidSecret 等）不能当作"没有旧凭据"，
        // 否则用户放弃时会走删除分支，误删已有条目；直接中止向导。
        match read_credential(&paths.credentials_file, spec.slug) {
            Ok(_) | Err(CredentialError::Missing) => {}
            Err(error) => {
                return Err(crate::Error::Config(format!(
                    "凭据文件 {} 已损坏或无法读取（{error}），请先修复或删除后重试",
                    paths.credentials_file.display()
                )));
            }
        }
        // 写入前做整体字节快照：v1 旧文件在首次 v2 写入时被迁移（旧 key 挂到
        // 保留 slug 下），条目级恢复会留下"新 slug → 旧 key"的多余条目；只有
        // 用快照整体还原才能回到写入前的精确状态。
        snapshot = snapshot_credentials(&paths.credentials_file);
        write_credential(&paths.credentials_file, spec.slug, new_credential)
            .map_err(|error| crate::Error::Config(error.to_string()))?;
    }
    // 写入凭据之后的每一步（连接测试、失败菜单、write_atomic、终端 I/O）都
    // 可能失败；任何 Err 返回前都必须恢复写入前的凭据，不能只给用户报错。
    if let Err(error) = finish(
        &mut wizard,
        &mut config,
        ai,
        paths,
        spec,
        auth,
        snapshot.as_deref().map(Vec::as_slice),
        written,
    ) {
        restore_credential(
            wizard.err,
            paths,
            spec.slug,
            snapshot.as_deref().map(Vec::as_slice),
            written,
        );
        return Err(error);
    }
    Ok(())
}

/// 连接测试、失败菜单与最终的配置写回。调用方保证：返回 Err 时恢复凭据。
#[expect(clippy::too_many_arguments)]
fn finish(
    wizard: &mut Wizard<'_>,
    config: &mut Config,
    ai: AiConfig,
    paths: &ConfigPaths,
    spec: &ProviderSpec,
    auth: AiAuth,
    snapshot: Option<&[u8]>,
    written: bool,
) -> crate::Result<()> {
    writeln!(wizard.output, "\n正在测试连接…")?;
    let save = 'decision: {
        loop {
            match (wizard.deps.connection_test)(&ai, &paths.credentials_file) {
                Ok(()) => {
                    writeln!(wizard.output, "✓ 连接成功")?;
                    break 'decision true;
                }
                Err(message) => {
                    writeln!(wizard.err, "✗ {message}")?;
                    loop {
                        match wizard.prompt_line("连接失败：r 重试 / s 仍然保存 / q 放弃: ")?
                        {
                            // q 或 EOF：放弃。
                            None => break 'decision false,
                            Some(answer) if answer.eq_ignore_ascii_case("r") => break,
                            Some(answer) if answer.eq_ignore_ascii_case("s") => {
                                break 'decision true;
                            }
                            Some(_) => writeln!(wizard.err, "请输入 r、s 或 q")?,
                        }
                    }
                }
            }
        }
    };
    if !save {
        restore_credential(wizard.err, paths, spec.slug, snapshot, written);
        writeln!(wizard.output, "已放弃，未保存配置。")?;
        return Ok(());
    }

    // 第六步：仅更新 [ai] 节后原子写回。
    config.ai = ai;
    config.write_atomic(&paths.config_file)?;

    writeln!(wizard.output, "\n配置完成：")?;
    writeln!(wizard.output, "  服务商: {}", spec.label)?;
    writeln!(wizard.output, "  认证方式: {}", auth_label(spec, auth))?;
    writeln!(wizard.output, "  端点: {}", config.ai.endpoint)?;
    writeln!(wizard.output, "  模型: {}", config.ai.model)?;
    if written {
        writeln!(
            wizard.output,
            "  凭据: {}（权限 0600）",
            paths.credentials_file.display()
        )?;
    }
    writeln!(
        wizard.output,
        "\n在命令前加 {} 即可向 AI 提问，例如 \"{}list large files\"。",
        config.ai.trigger_prefix, config.ai.trigger_prefix
    )?;
    Ok(())
}

/// 写入前对整个凭据文件做字节级快照（用于放弃时整体还原）；读不到或超限
/// 视为"没有可用快照"，恢复时退化为条目级删除（尽力而为）。
fn snapshot_credentials(path: &std::path::Path) -> Option<Zeroizing<Vec<u8>>> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(SNAPSHOT_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > SNAPSHOT_MAX_BYTES {
        return None;
    }
    Some(bytes)
}

/// 把快照字节安全写回凭据文件（临时文件 + persist + 0600 + fsync），与
/// `credentials` 模块原子写的语义一致。
fn write_snapshot(path: &std::path::Path, bytes: &[u8]) -> Result<(), CredentialError> {
    let parent = path.parent().ok_or(CredentialError::InvalidFormat)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".credentials.toml.")
        .tempfile_in(parent)?;
    set_snapshot_permissions(temporary.as_file())?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn set_snapshot_permissions(file: &std::fs::File) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_snapshot_permissions(_: &std::fs::File) -> Result<(), CredentialError> {
    Ok(())
}

/// 放弃时把凭据存储恢复到写入前的状态：有快照则整体还原（同时覆盖 v1→v2
/// 迁移残留），没有快照（原本无文件）则删除新条目。
fn restore_credential(
    err: &mut dyn Write,
    paths: &ConfigPaths,
    slug: &str,
    snapshot: Option<&[u8]>,
    written: bool,
) {
    if !written {
        return;
    }
    let result = match snapshot {
        Some(bytes) => write_snapshot(&paths.credentials_file, bytes),
        None => delete_credential(&paths.credentials_file, slug),
    };
    // 尽力恢复；Missing 说明条目本就不存在，无需报告。
    if let Err(error) = result
        && !matches!(error, CredentialError::Missing)
    {
        let _ = writeln!(err, "恢复凭据失败：{error}");
    }
}

fn auth_label(spec: &ProviderSpec, auth: AiAuth) -> &'static str {
    if spec.auth_methods.is_empty() {
        "无"
    } else {
        match auth {
            AiAuth::ApiKey => "API Key",
            AiAuth::OAuth => "OAuth",
        }
    }
}

struct Wizard<'a> {
    input: &'a mut dyn BufRead,
    output: &'a mut dyn Write,
    err: &'a mut dyn Write,
    deps: Deps,
}

impl Wizard<'_> {
    fn cancelled(&mut self) -> crate::Result<()> {
        writeln!(self.output, "已取消，未写入任何配置。")?;
        Ok(())
    }

    /// 打印提示并读取一行（裁剪空白）；`None` 表示用户输入 q 或 EOF（退出向导）。
    fn prompt_line(&mut self, prompt: &str) -> crate::Result<Option<String>> {
        write!(self.output, "{prompt}")?;
        self.output.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            writeln!(self.output)?;
            return Ok(None);
        }
        let answer = line.trim();
        if answer.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        Ok(Some(answer.to_owned()))
    }

    /// 读取密钥原文（只去掉行尾换行，不做 trim）；q 与 EOF 同样退出。
    fn prompt_secret(&mut self, prompt: &str) -> crate::Result<Option<String>> {
        write!(self.output, "{prompt}")?;
        self.output.flush()?;
        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            writeln!(self.output)?;
            return Ok(None);
        }
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        if line.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        Ok(Some(line))
    }

    fn choose_provider(
        &mut self,
        current: Option<&str>,
    ) -> crate::Result<Option<&'static ProviderSpec>> {
        let registry = providers::registry();
        writeln!(self.output, "\n选择 AI 服务商（输入编号）：")?;
        for (index, spec) in registry.iter().enumerate() {
            let marker = if current == Some(spec.slug) {
                " (current)"
            } else {
                ""
            };
            writeln!(
                self.output,
                "  {}) {} — {}{}",
                index + 1,
                spec.label,
                spec.description,
                marker
            )?;
        }
        let default = current
            .and_then(|slug| registry.iter().position(|spec| spec.slug == slug))
            .map(|index| index + 1);
        loop {
            let prompt = match default {
                Some(number) => format!("服务商 [{number}]: "),
                None => "服务商: ".to_owned(),
            };
            let Some(answer) = self.prompt_line(&prompt)? else {
                return Ok(None);
            };
            let choice = if answer.is_empty() {
                default
            } else {
                answer
                    .parse::<usize>()
                    .ok()
                    .filter(|number| (1..=registry.len()).contains(number))
            };
            match choice {
                Some(number) => return Ok(Some(&registry[number - 1])),
                None => writeln!(self.err, "无效选择，请输入 1-{} 或 q", registry.len())?,
            }
        }
    }

    fn choose_auth(&mut self, spec: &ProviderSpec) -> crate::Result<Option<AiAuth>> {
        match spec.auth_methods {
            // ollama：无凭据，仅占位。
            [] => Ok(Some(AiAuth::ApiKey)),
            [only] => Ok(Some(*only)),
            methods => {
                writeln!(self.output, "\n选择认证方式：")?;
                for (index, method) in methods.iter().enumerate() {
                    let label = match method {
                        AiAuth::OAuth => "OAuth（浏览器/设备授权登录）",
                        AiAuth::ApiKey => "API Key",
                    };
                    writeln!(self.output, "  {}) {label}", index + 1)?;
                }
                loop {
                    let Some(answer) = self.prompt_line("认证方式 [1]: ")? else {
                        return Ok(None);
                    };
                    let choice = if answer.is_empty() {
                        Some(1)
                    } else {
                        answer
                            .parse::<usize>()
                            .ok()
                            .filter(|number| (1..=methods.len()).contains(number))
                    };
                    match choice {
                        Some(number) => return Ok(Some(methods[number - 1])),
                        None => {
                            writeln!(self.err, "无效选择，请输入 1-{} 或 q", methods.len())?;
                        }
                    }
                }
            }
        }
    }

    /// `custom` 服务商的端点输入：必须是 http(s) 绝对 URL，不含用户信息、
    /// 查询参数或 fragment；缺少 scheme 时自动补 `https://`。
    fn prompt_endpoint(&mut self) -> crate::Result<Option<String>> {
        loop {
            let Some(answer) =
                self.prompt_line("端点 URL（OpenAI 兼容，例如 https://api.example.com/v1）: ")?
            else {
                return Ok(None);
            };
            if answer.is_empty() {
                writeln!(self.err, "端点不能为空")?;
                continue;
            }
            let candidate = if answer.contains("://") {
                answer
            } else {
                format!("https://{answer}")
            };
            match validate_endpoint(&candidate) {
                Ok(()) => return Ok(Some(candidate)),
                Err(message) => writeln!(self.err, "{message}")?,
            }
        }
    }

    fn collect_api_key(&mut self, spec: &ProviderSpec) -> crate::Result<Option<Zeroizing<String>>> {
        if !spec.env_hint.is_empty()
            && let Some(value) = (self.deps.env_get)(spec.env_hint)
            && validate_secret(&value).is_ok()
        {
            writeln!(self.output, "\n检测到环境变量 ${} 已设置。", spec.env_hint)?;
            loop {
                let Some(answer) = self.prompt_line("使用该密钥？[Y/n]: ")? else {
                    return Ok(None);
                };
                match answer.to_ascii_lowercase().as_str() {
                    "" | "y" | "yes" => return Ok(Some(Zeroizing::new(value))),
                    "n" | "no" => break,
                    _ => writeln!(self.err, "请输入 y 或 n")?,
                }
            }
        }
        writeln!(self.output, "\n请输入 {} 的 API Key。", spec.label)?;
        if !spec.env_hint.is_empty() {
            writeln!(
                self.output,
                "（也可以先设置环境变量 ${} 再运行向导）",
                spec.env_hint
            )?;
        }
        for attempt in 0..MAX_SECRET_ATTEMPTS {
            let Some(key) = self.prompt_secret("API Key: ")? else {
                return Ok(None);
            };
            if validate_secret(&key).is_ok() {
                return Ok(Some(Zeroizing::new(key)));
            }
            if attempt + 1 < MAX_SECRET_ATTEMPTS {
                writeln!(
                    self.err,
                    "密钥无效（不能为空、不能含首尾空白或控制字符），请重试"
                )?;
            } else {
                writeln!(self.err, "密钥无效，已取消")?;
            }
        }
        Ok(None)
    }

    fn collect_oauth(&mut self, spec: &ProviderSpec) -> crate::Result<Option<OAuthTokens>> {
        let result = if spec.slug == "gemini-oauth" {
            let output = &mut *self.output;
            let mut sink = move |url: String| {
                let _ = write!(
                    output,
                    "\n请在浏览器打开以下链接完成 Google 授权：\n{url}\n授权后页面会显示一串代码，请粘贴到下面。\n授权代码: "
                );
                let _ = output.flush();
            };
            let input = &mut *self.input;
            let mut read_code = move || -> Result<String, OAuthError> {
                let mut line = String::new();
                match input.read_line(&mut line) {
                    // EOF 视为取消，与 Ctrl-C（默认 SIGINT 终止进程）一致。
                    Ok(0) => Err(OAuthError::Cancelled),
                    Ok(_) => {
                        let code = line.trim();
                        if code.eq_ignore_ascii_case("q") {
                            Err(OAuthError::Cancelled)
                        } else {
                            Ok(code.to_owned())
                        }
                    }
                    Err(_) => Err(OAuthError::Cancelled),
                }
            };
            (self.deps.gemini_flow)(&mut sink, &mut read_code)
        } else {
            let output = &mut *self.output;
            let mut sink = move |prompt: DevicePrompt| {
                let _ = writeln!(
                    output,
                    "\n请在浏览器打开: {}\n并输入代码: {}\n等待授权中（Ctrl-C 取消）…",
                    prompt.verification_uri, prompt.user_code
                );
                let _ = output.flush();
            };
            (self.deps.device_flow)(spec.slug, &mut sink)
        };
        match result {
            Ok(tokens) => Ok(Some(tokens)),
            Err(OAuthError::Cancelled) => Ok(None),
            Err(error) => Err(crate::Error::Config(format!(
                "OAuth 登录失败：{} {}",
                error.code(),
                error
            ))),
        }
    }

    fn choose_model(
        &mut self,
        spec: &ProviderSpec,
        query: &ModelListQuery<'_>,
        current_model: Option<&str>,
    ) -> crate::Result<Option<String>> {
        writeln!(self.output, "\n正在获取模型列表…")?;
        let models: Vec<String> = match (self.deps.list_models)(query) {
            Some(list) => list,
            None => spec
                .default_models
                .iter()
                .map(|model| (*model).to_owned())
                .collect(),
        }
        // 远程来源的模型名可能夹带控制字符（ANSI escape 注入），一律丢弃。
        .into_iter()
        .filter(|name| !name.chars().any(char::is_control))
        .collect();
        if models.is_empty() {
            writeln!(self.output, "未能获取模型列表，请手动输入。")?;
            return self.prompt_model_name();
        }
        let default = match current_model {
            Some(current) => current.to_owned(),
            // models 非空，first() 必然有值。
            None => models.first().cloned().unwrap_or_default(),
        };
        writeln!(self.output, "选择模型（输入编号，m 手动输入）：")?;
        for (index, model) in models.iter().enumerate() {
            writeln!(self.output, "  {}) {model}", index + 1)?;
        }
        loop {
            let Some(answer) = self.prompt_line(&format!("模型 [{default}]: "))? else {
                return Ok(None);
            };
            if answer.is_empty() {
                return Ok(Some(default));
            }
            if answer.eq_ignore_ascii_case("m") {
                return self.prompt_model_name();
            }
            match answer
                .parse::<usize>()
                .ok()
                .filter(|number| (1..=models.len()).contains(number))
            {
                Some(number) => return Ok(Some(models[number - 1].clone())),
                None => writeln!(self.err, "无效选择，请输入 1-{}、m 或 q", models.len())?,
            }
        }
    }

    fn prompt_model_name(&mut self) -> crate::Result<Option<String>> {
        loop {
            let Some(name) = self.prompt_line("模型名称: ")? else {
                return Ok(None);
            };
            if name.is_empty() {
                writeln!(self.err, "模型名称不能为空")?;
                continue;
            }
            // 模型名会原样打印并写入配置，拒绝控制字符（ANSI escape 注入）。
            if name.chars().any(char::is_control) {
                writeln!(self.err, "模型名称不能包含控制字符")?;
                continue;
            }
            return Ok(Some(name));
        }
    }
}

fn validate_endpoint(raw: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(raw).map_err(|_| {
        "端点必须是合法的 http(s) 绝对 URL（例如 https://api.example.com/v1）".to_owned()
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("端点必须是 http(s) 绝对 URL".to_owned());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("端点不能包含用户名/密码、查询参数或 fragment".to_owned());
    }
    Ok(())
}
