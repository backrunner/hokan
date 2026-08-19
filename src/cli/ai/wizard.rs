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

/// The setup wizard is line-oriented so it remains usable over SSH and with
/// pasted OAuth codes, but its output still benefits from a small visual
/// hierarchy. Tests use the plain renderer; the real TTY opts into this theme.
#[derive(Clone, Copy)]
struct Theme {
    enabled: bool,
}

#[derive(Clone, Copy)]
enum Tone {
    Accent,
    Heading,
    Prompt,
    Muted,
    Success,
    Warning,
    Error,
    Value,
}

impl Theme {
    const fn tty(enabled: bool) -> Self {
        Self { enabled }
    }

    fn paint(self, tone: Tone, text: impl std::fmt::Display) -> String {
        let text = text.to_string();
        if !self.enabled {
            return text;
        }
        let code = match tone {
            Tone::Accent => "1;36",
            Tone::Heading => "1;35",
            Tone::Prompt => "1;33",
            Tone::Muted => "90",
            Tone::Success => "1;32",
            Tone::Warning => "1;33",
            Tone::Error => "1;31",
            Tone::Value => "36",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }

    fn divider(self) -> String {
        self.paint(
            Tone::Muted,
            "────────────────────────────────────────────────────────",
        )
    }
}

#[cfg(test)]
pub(super) fn run_with_io(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    err: &mut dyn Write,
    paths: &ConfigPaths,
    deps: Deps,
) -> crate::Result<()> {
    run_with_io_mode(input, output, err, paths, deps, false)
}

/// Production entry point. `run` has already verified that stdin/stdout are
/// terminals, so this can honor the user's `ui.color` preference without
/// changing the test-friendly plain `run_with_io` helper.
pub(super) fn run_with_io_tty(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    err: &mut dyn Write,
    paths: &ConfigPaths,
    deps: Deps,
) -> crate::Result<()> {
    run_with_io_mode(input, output, err, paths, deps, true)
}

fn run_with_io_mode(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    err: &mut dyn Write,
    paths: &ConfigPaths,
    deps: Deps,
    tty: bool,
) -> crate::Result<()> {
    // 先加载配置：解析失败立即中止，避免覆盖损坏的配置文件。
    let mut config = Config::load(&paths.config_file)?;
    let color = tty
        && match config.ui.color.as_str() {
            "always" => true,
            "never" => false,
            _ => std::env::var_os("NO_COLOR").is_none(),
        };
    let current_provider = (config.ai.enabled && !config.ai.provider.trim().is_empty())
        .then(|| config.ai.provider.trim().to_owned());
    let current_model = config.ai.model.trim().to_owned();

    let mut wizard = Wizard {
        input,
        output,
        err,
        deps,
        theme: Theme::tty(color),
        tty,
    };
    wizard.banner(current_provider.as_deref(), Some(current_model.as_str()))?;

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

    // 第三步：凭据。即使本地服务无需凭据，也显示完整进度，避免从 2/6
    // 直接跳到 4/6；环境变量命中时同样先说明凭据的保存方式。
    let credential_detail = if spec.auth_methods.is_empty() {
        "此本地服务无需凭据"
    } else {
        match auth {
            AiAuth::ApiKey => "密钥仅写入本机 credentials.toml（权限 0600）",
            AiAuth::OAuth => "使用浏览器或设备授权登录，不显示访问令牌",
        }
    };
    wizard.step(3, "配置凭据", credential_detail)?;
    let credential: Option<ProviderCredential> = if spec.auth_methods.is_empty() {
        wizard.info("无需配置凭据，继续选择模型")?;
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
    // Managed setup always copies API keys into credentials.toml. Clear any
    // legacy environment source so the resulting config describes the source
    // it actually uses (and leaves OAuth/no-auth providers free of stale data).
    ai.api_key_env.clear();
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
            wizard.theme,
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
    wizard.step(5, "连接测试", "确认凭据可用，再保存设置")?;
    wizard.info("正在测试连接…")?;
    let save = 'decision: {
        loop {
            match (wizard.deps.connection_test)(&ai, &paths.credentials_file) {
                Ok(()) => {
                    wizard.success("连接成功")?;
                    break 'decision true;
                }
                Err(message) => {
                    wizard.error(message)?;
                    loop {
                        match wizard.prompt_line("连接失败：r 重试 / s 仍然保存 / q 放弃: ")?
                        {
                            // q 或 EOF：放弃。
                            None => break 'decision false,
                            Some(answer) if answer.eq_ignore_ascii_case("r") => break,
                            Some(answer) if answer.eq_ignore_ascii_case("s") => {
                                break 'decision true;
                            }
                            Some(_) => wizard.error("请输入 r、s 或 q")?,
                        }
                    }
                }
            }
        }
    };
    if !save {
        restore_credential(
            wizard.err,
            wizard.theme,
            paths,
            spec.slug,
            snapshot,
            written,
        );
        wizard.warning("已放弃，未保存配置。")?;
        return Ok(());
    }

    // 第六步：仅更新 [ai] 节后原子写回。
    config.ai = ai;
    config.write_atomic(&paths.config_file)?;

    wizard.step(6, "完成", "设置已写入本地配置")?;
    wizard.success("配置完成")?;
    wizard.line(Tone::Heading, format!("  服务商   {}", spec.label))?;
    wizard.line(
        Tone::Heading,
        format!("  认证方式 {}", auth_label(spec, auth)),
    )?;
    wizard.line(Tone::Value, format!("  端点     {}", config.ai.endpoint))?;
    wizard.line(Tone::Value, format!("  模型     {}", config.ai.model))?;
    if written {
        wizard.line(
            Tone::Value,
            format!(
                "  凭据     {}（权限 0600）",
                paths.credentials_file.display()
            ),
        )?;
    }
    wizard.line(
        Tone::Muted,
        format!(
            "\n在命令前加 {} 即可向 AI 提问，例如 \"{}list large files\"。",
            config.ai.trigger_prefix, config.ai.trigger_prefix
        ),
    )?;
    wizard.line(Tone::Muted, wizard.theme.divider())?;
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
    theme: Theme,
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
        let _ = writeln!(
            err,
            "{}",
            theme.paint(Tone::Error, format!("✗ 恢复凭据失败：{error}"))
        );
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

fn provider_section(slug: &str) -> Option<&'static str> {
    match slug {
        "deepseek" => Some("常用与账号登录"),
        "ollama" => Some("本地与自定义"),
        "opencode-go" => Some("订阅与聚合网关"),
        "openai-api" => Some("常用 API 平台"),
        "alibaba-coding-plan" => Some("订阅与云网关"),
        "huggingface" => Some("更多托管平台"),
        _ => None,
    }
}

fn provider_auth_badge(spec: &ProviderSpec) -> &'static str {
    match spec.auth_methods {
        [] => "[本地]",
        [AiAuth::OAuth] => "[OAuth]",
        [AiAuth::ApiKey] => "[API Key]",
        _ => "[多种认证]",
    }
}

struct Wizard<'a> {
    input: &'a mut dyn BufRead,
    output: &'a mut dyn Write,
    err: &'a mut dyn Write,
    deps: Deps,
    theme: Theme,
    tty: bool,
}

#[cfg(unix)]
struct SecretEchoGuard {
    tty: std::fs::File,
    original: nix::sys::termios::Termios,
    _signals: SecretSignalGuard,
}

#[cfg(unix)]
struct SecretSignalGuard {
    original: nix::sys::signal::SigSet,
}

#[cfg(unix)]
impl SecretSignalGuard {
    fn block() -> std::io::Result<Self> {
        use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};

        let mut blocked = SigSet::empty();
        for signal in [
            Signal::SIGHUP,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGTERM,
            Signal::SIGTSTP,
        ] {
            blocked.add(signal);
        }
        let mut original = SigSet::empty();
        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&blocked), Some(&mut original))
            .map_err(std::io::Error::other)?;
        Ok(Self { original })
    }
}

#[cfg(unix)]
impl Drop for SecretSignalGuard {
    fn drop(&mut self) {
        use nix::sys::signal::{SigmaskHow, pthread_sigmask};

        let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&self.original), None);
    }
}

#[cfg(unix)]
impl SecretEchoGuard {
    fn disable() -> std::io::Result<Self> {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")?;
        Self::for_tty(tty)
    }

    fn for_tty(tty: std::fs::File) -> std::io::Result<Self> {
        use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};

        let signals = SecretSignalGuard::block()?;
        let original = tcgetattr(&tty).map_err(std::io::Error::other)?;
        let mut hidden = original.clone();
        hidden
            .local_flags
            .remove(LocalFlags::ECHO | LocalFlags::ECHONL);
        tcsetattr(&tty, SetArg::TCSANOW, &hidden).map_err(std::io::Error::other)?;
        Ok(Self {
            tty,
            original,
            _signals: signals,
        })
    }
}

#[cfg(unix)]
impl Drop for SecretEchoGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{SetArg, tcsetattr};

        let _ = tcsetattr(&self.tty, SetArg::TCSANOW, &self.original);
    }
}

impl Wizard<'_> {
    fn line(&mut self, tone: Tone, text: impl std::fmt::Display) -> crate::Result<()> {
        let rendered = self.theme.paint(tone, text);
        writeln!(self.output, "{rendered}")?;
        Ok(())
    }

    fn info(&mut self, text: impl std::fmt::Display) -> crate::Result<()> {
        self.line(Tone::Accent, format!("• {text}"))
    }

    fn success(&mut self, text: impl std::fmt::Display) -> crate::Result<()> {
        self.line(Tone::Success, format!("✓ {text}"))
    }

    fn warning(&mut self, text: impl std::fmt::Display) -> crate::Result<()> {
        self.line(Tone::Warning, format!("! {text}"))
    }

    fn error(&mut self, text: impl std::fmt::Display) -> crate::Result<()> {
        let rendered = self.theme.paint(Tone::Error, format!("✗ {text}"));
        writeln!(self.err, "{rendered}")?;
        Ok(())
    }

    fn step(&mut self, number: usize, title: &str, detail: &str) -> crate::Result<()> {
        let label = self
            .theme
            .paint(Tone::Heading, format!("[{number}/6] {title}"));
        let detail = self.theme.paint(Tone::Muted, detail);
        writeln!(
            self.output,
            "\n{}\n  {label}\n  {detail}",
            self.theme.divider()
        )?;
        Ok(())
    }

    fn banner(
        &mut self,
        current_provider: Option<&str>,
        current_model: Option<&str>,
    ) -> crate::Result<()> {
        let title = self.theme.paint(Tone::Accent, "◆ hokan AI 设置");
        let provider = current_provider.unwrap_or("尚未配置");
        let model = current_model
            .filter(|model| !model.is_empty())
            .unwrap_or("-");
        let status = if current_provider.is_some() {
            self.theme.paint(Tone::Success, "已启用")
        } else {
            self.theme.paint(Tone::Muted, "未启用")
        };
        writeln!(
            self.output,
            "\n{title}\n{}\n  当前状态  {status}\n  服务商    {}\n  模型      {}\n{}\n  {}\n",
            self.theme.divider(),
            self.theme.paint(Tone::Value, provider),
            self.theme.paint(Tone::Value, model),
            self.theme.divider(),
            self.theme.paint(
                Tone::Muted,
                "回车使用默认值 · q 随时取消 · 密钥仅保存到 0600 凭据文件"
            )
        )?;
        Ok(())
    }

    fn cancelled(&mut self) -> crate::Result<()> {
        self.warning("已取消，未写入任何配置。")
    }

    /// 打印提示并读取一行（裁剪空白）；`None` 表示用户输入 q 或 EOF（退出向导）。
    fn prompt_line(&mut self, prompt: &str) -> crate::Result<Option<String>> {
        let rendered = self.theme.paint(Tone::Prompt, prompt);
        write!(self.output, "{rendered}")?;
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

    /// 读取密钥原文（只去掉行尾换行，不做 trim）；真实 Unix TTY 会在读取
    /// 期间关闭回显，并在所有退出路径恢复；q 与 EOF 同样退出。
    fn prompt_secret(&mut self, prompt: &str) -> crate::Result<Option<String>> {
        let rendered = self.theme.paint(Tone::Prompt, prompt);
        write!(self.output, "{rendered}")?;
        self.output.flush()?;
        let mut line = String::new();
        #[cfg(unix)]
        let echo_guard = self.tty.then(SecretEchoGuard::disable).transpose()?;
        let read = self.input.read_line(&mut line);
        #[cfg(unix)]
        drop(echo_guard);
        let read = read?;
        if self.tty {
            writeln!(self.output)?;
        }
        if read == 0 {
            if !self.tty {
                writeln!(self.output)?;
            }
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
        self.step(1, "选择服务商", "选择一个云端、订阅或本地 AI 服务")?;
        for (index, spec) in registry.iter().enumerate() {
            if let Some(section) = provider_section(spec.slug) {
                self.line(Tone::Heading, format!("\n  {section}"))?;
            }
            let marker = if current == Some(spec.slug) {
                "  ← 当前"
            } else {
                ""
            };
            let row = format!(
                "  {:>2}) {}  {}{}",
                index + 1,
                spec.label,
                provider_auth_badge(spec),
                marker
            );
            self.line(
                if current == Some(spec.slug) {
                    Tone::Success
                } else {
                    Tone::Value
                },
                row,
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
                Some(number) => {
                    let spec = &registry[number - 1];
                    self.info(format!("已选择 {}：{}", spec.label, spec.description))?;
                    return Ok(Some(spec));
                }
                None => self.error(format!("无效选择，请输入 1-{} 或 q", registry.len()))?,
            }
        }
    }

    fn choose_auth(&mut self, spec: &ProviderSpec) -> crate::Result<Option<AiAuth>> {
        let auth_detail = match spec.auth_methods {
            [] => "此服务商不需要凭据",
            [AiAuth::OAuth] => "使用浏览器或设备授权登录",
            [AiAuth::ApiKey] => "使用服务商签发的 API Key",
            _ => "选择浏览器登录或 API Key",
        };
        self.step(2, "选择认证方式", auth_detail)?;
        match spec.auth_methods {
            // ollama：无凭据，仅占位。
            [] => {
                self.line(Tone::Value, "  认证方式  无需凭据")?;
                Ok(Some(AiAuth::ApiKey))
            }
            [only] => {
                let label = match only {
                    AiAuth::OAuth => "OAuth（浏览器/设备授权登录）",
                    AiAuth::ApiKey => "API Key",
                };
                self.line(Tone::Value, format!("  认证方式  {label}"))?;
                Ok(Some(*only))
            }
            methods => {
                for (index, method) in methods.iter().enumerate() {
                    let label = match method {
                        AiAuth::OAuth => "OAuth（浏览器/设备授权登录）",
                        AiAuth::ApiKey => "API Key",
                    };
                    self.line(Tone::Value, format!("  {}) {label}", index + 1))?;
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
                        None => self.error(format!("无效选择，请输入 1-{} 或 q", methods.len()))?,
                    }
                }
            }
        }
    }

    /// `custom` 服务商的端点输入：必须是 http(s) 绝对 URL，不含用户信息、
    /// 查询参数或 fragment；缺少 scheme 时自动补 `https://`。
    fn prompt_endpoint(&mut self) -> crate::Result<Option<String>> {
        self.info("这是一个 OpenAI-compatible 地址，可以只填写域名，自动补 https://")?;
        loop {
            let Some(answer) =
                self.prompt_line("端点 URL（OpenAI 兼容，例如 https://api.example.com/v1）: ")?
            else {
                return Ok(None);
            };
            if answer.is_empty() {
                self.error("端点不能为空")?;
                continue;
            }
            let candidate = if answer.contains("://") {
                answer
            } else {
                format!("https://{answer}")
            };
            match validate_endpoint(&candidate) {
                Ok(()) => return Ok(Some(candidate)),
                Err(message) => self.error(message)?,
            }
        }
    }

    fn collect_api_key(&mut self, spec: &ProviderSpec) -> crate::Result<Option<Zeroizing<String>>> {
        let detected = std::iter::once(spec.env_hint)
            .chain(spec.env_aliases.iter().copied())
            .filter(|name| !name.is_empty())
            .find_map(|name| {
                let value = (self.deps.env_get)(name)?;
                validate_secret(&value).ok().map(|()| (name, value))
            });
        if let Some((name, value)) = detected {
            self.info(format!("检测到环境变量 ${name} 已设置"))?;
            loop {
                let Some(answer) = self.prompt_line("使用该密钥？[Y/n]: ")? else {
                    return Ok(None);
                };
                match answer.to_ascii_lowercase().as_str() {
                    "" | "y" | "yes" => return Ok(Some(Zeroizing::new(value))),
                    "n" | "no" => break,
                    _ => self.error("请输入 y 或 n")?,
                }
            }
        }
        self.info(format!("请输入 {} 的 API Key", spec.label))?;
        if !spec.env_hint.is_empty() {
            let aliases = if spec.env_aliases.is_empty() {
                String::new()
            } else {
                format!(
                    "（也支持 {}）",
                    spec.env_aliases
                        .iter()
                        .map(|name| format!("${name}"))
                        .collect::<Vec<_>>()
                        .join("、")
                )
            };
            self.line(
                Tone::Muted,
                format!(
                    "  也可以先设置环境变量 ${}{}，向导会自动检测",
                    spec.env_hint, aliases
                ),
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
                self.error("密钥无效（不能为空、不能含首尾空白或控制字符），请重试")?;
            } else {
                self.error("密钥无效，已取消")?;
            }
        }
        Ok(None)
    }

    fn collect_oauth(&mut self, spec: &ProviderSpec) -> crate::Result<Option<OAuthTokens>> {
        let result = if spec.slug == "gemini-oauth" {
            let output = &mut *self.output;
            let theme = self.theme;
            let mut sink = move |url: String| {
                let _ = write!(output, "{}", theme.paint(
                    Tone::Accent,
                    format!(
                        "\n请在浏览器打开以下链接完成 Google 授权：\n{url}\n授权后页面会显示一串代码，请粘贴到下面。\n授权代码: "
                    ),
                ));
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
            let theme = self.theme;
            let mut sink =
                move |prompt: DevicePrompt| {
                    let _ =
                        writeln!(output, "{}", theme.paint(
                    Tone::Accent,
                    format!(
                        "\n请在浏览器打开: {}\n并输入代码: {}\n等待授权中（Ctrl-C 取消）…",
                        prompt.verification_uri, prompt.user_code
                    ),
                ));
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
        self.step(4, "选择模型", "先尝试在线读取，失败时使用内置推荐")?;
        let live_models = if spec.supports_model_listing {
            self.info("正在获取模型列表…")?;
            (self.deps.list_models)(query)
        } else {
            self.info("此服务商没有通用模型列表接口，使用内置推荐")?;
            None
        };
        let models: Vec<String> = match live_models {
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
            self.warning("未能获取模型列表，请手动输入")?;
            return self.prompt_model_name();
        }
        let default = match current_model {
            Some(current) => current.to_owned(),
            // models 非空，first() 必然有值。
            None => models.first().cloned().unwrap_or_default(),
        };
        self.line(Tone::Muted, "输入编号选择，m 手动输入")?;
        for (index, model) in models.iter().enumerate() {
            self.line(Tone::Value, format!("  {:>2}) {model}", index + 1))?;
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
                None => self.error(format!("无效选择，请输入 1-{}、m 或 q", models.len()))?,
            }
        }
    }

    fn prompt_model_name(&mut self) -> crate::Result<Option<String>> {
        loop {
            let Some(name) = self.prompt_line("模型名称: ")? else {
                return Ok(None);
            };
            if name.is_empty() {
                self.error("模型名称不能为空")?;
                continue;
            }
            // 模型名会原样打印并写入配置，拒绝控制字符（ANSI escape 注入）。
            if name.chars().any(char::is_control) {
                self.error("模型名称不能包含控制字符")?;
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

#[cfg(test)]
mod theme_tests {
    use super::{Theme, Tone};

    #[test]
    fn tty_theme_distinguishes_status_tones() {
        let theme = Theme::tty(true);
        assert_eq!(theme.paint(Tone::Success, "ok"), "\x1b[1;32mok\x1b[0m");
        assert_eq!(theme.paint(Tone::Error, "bad"), "\x1b[1;31mbad\x1b[0m");
        assert_eq!(theme.paint(Tone::Muted, "hint"), "\x1b[90mhint\x1b[0m");
    }

    #[test]
    fn plain_theme_never_emits_terminal_escapes() {
        let theme = Theme::tty(false);
        for tone in [Tone::Success, Tone::Error, Tone::Muted, Tone::Prompt] {
            assert_eq!(theme.paint(tone, "text"), "text");
        }
    }
}

#[cfg(all(test, unix))]
mod echo_tests {
    use std::fs::File;

    use nix::{
        pty::openpty,
        sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask},
        sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr},
    };

    use super::SecretEchoGuard;

    #[test]
    fn secret_echo_guard_disables_and_restores_echo() {
        let pty = openpty(None, None).expect("open pty");
        let slave = File::from(pty.slave);
        let observer = slave.try_clone().expect("clone pty slave");
        let mut signal_mask_before = SigSet::empty();
        pthread_sigmask(SigmaskHow::SIG_BLOCK, None, Some(&mut signal_mask_before))
            .expect("read signal mask");
        let mut original = tcgetattr(&observer).expect("read termios");
        original
            .local_flags
            .insert(LocalFlags::ECHO | LocalFlags::ECHONL);
        tcsetattr(&observer, SetArg::TCSANOW, &original).expect("enable echo");

        let guard = SecretEchoGuard::for_tty(slave).expect("disable echo");
        let hidden = tcgetattr(&observer).expect("read hidden termios");
        assert!(!hidden.local_flags.contains(LocalFlags::ECHO));
        assert!(!hidden.local_flags.contains(LocalFlags::ECHONL));
        let mut signal_mask_hidden = SigSet::empty();
        pthread_sigmask(SigmaskHow::SIG_BLOCK, None, Some(&mut signal_mask_hidden))
            .expect("read hidden signal mask");
        for signal in [
            Signal::SIGHUP,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGTERM,
            Signal::SIGTSTP,
        ] {
            assert!(signal_mask_hidden.contains(signal));
        }

        drop(guard);
        let restored = tcgetattr(&observer).expect("read restored termios");
        assert!(restored.local_flags.contains(LocalFlags::ECHO));
        assert!(restored.local_flags.contains(LocalFlags::ECHONL));
        let mut signal_mask_restored = SigSet::empty();
        pthread_sigmask(SigmaskHow::SIG_BLOCK, None, Some(&mut signal_mask_restored))
            .expect("read restored signal mask");
        for signal in [
            Signal::SIGHUP,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGTERM,
            Signal::SIGTSTP,
        ] {
            assert_eq!(
                signal_mask_restored.contains(signal),
                signal_mask_before.contains(signal)
            );
        }
    }
}
