//! `hokan ai setup`：行内交互式向导，引导用户完成 AI 服务商与凭据配置。
//!
//! 设计要点：
//! - 向导直接读写标准输入/输出（即时回显），不走 `cli::run` 的缓冲输出；
//!   仅在 stdin 与 stdout 都是终端时运行，脚本化场景请用 `hokan config ai`。
//! - 凭据先写入 credentials.toml 再做连接测试（`AiClient` 只能从磁盘读取凭据）；
//!   写入前对整个凭据文件做字节快照，之后任何失败（连接失败选"放弃"、终端 I/O
//!   错误、配置写回失败）都用快照整体还原写入前的状态（原本无文件则删除新条目）。
//! - 任何输出或错误信息都不包含 secret。

use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};

use super::AiCommand;
use crate::{
    ai::{DevicePrompt, OAuthError},
    config::{AiConfig, ConfigPaths, OAuthTokens},
};

mod models;
#[cfg(test)]
mod tests;
mod wizard;

use models::list_models_live;
use wizard::run_with_io;

pub fn run(command: AiCommand) -> crate::Result<()> {
    match command {
        AiCommand::Setup => {
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                return Err(crate::Error::Config(
                    "`hokan ai setup` 是交互式向导，需要在终端中运行；脚本化配置请使用 \
                     `hokan config ai --enable --endpoint <URL> --model <名称> --api-key-stdin`"
                        .into(),
                ));
            }
            let paths = ConfigPaths::discover()?;
            let mut input = std::io::stdin().lock();
            let mut output = std::io::stdout().lock();
            let mut err = std::io::stderr().lock();
            run_with_io(&mut input, &mut output, &mut err, &paths, production_deps())
        }
    }
}

/// 测试注入点：OAuth 流程、模型列表、连接测试与环境变量全部可替换，
/// 使 `run_with_io` 的测试完全不接触网络、TTY 与进程环境。
type DeviceFlowFn =
    Box<dyn FnMut(&str, &mut dyn FnMut(DevicePrompt)) -> Result<OAuthTokens, OAuthError>>;
type GeminiFlowFn = Box<
    dyn FnMut(
        &mut dyn FnMut(String),
        &mut dyn FnMut() -> Result<String, OAuthError>,
    ) -> Result<OAuthTokens, OAuthError>,
>;
/// `None` 表示拉取失败（回退静态表）；`Some([])` 表示成功但没有模型。
type ListModelsFn = Box<dyn FnMut(&ModelListQuery<'_>) -> Option<Vec<String>>>;
/// 成功为 `Ok(())`；失败为 `"<HK-… 错误码> <一句话>"`，不含 secret。
type ConnectionTestFn = Box<dyn FnMut(&AiConfig, &Path) -> Result<(), String>>;
type EnvGetFn = Box<dyn Fn(&str) -> Option<String>>;

struct Deps {
    device_flow: DeviceFlowFn,
    gemini_flow: GeminiFlowFn,
    list_models: ListModelsFn,
    connection_test: ConnectionTestFn,
    env_get: EnvGetFn,
}

/// 一次在线模型列表请求所需的全部参数。
struct ModelListQuery<'a> {
    slug: &'a str,
    endpoint: &'a str,
    bearer: Option<&'a str>,
    account_id: Option<&'a str>,
}

fn production_deps() -> Deps {
    Deps {
        device_flow: Box::new(|slug, sink| match slug {
            "openai-oauth" => crate::ai::run_codex_device_flow(sink),
            _ => crate::ai::run_grok_device_flow(sink),
        }),
        gemini_flow: Box::new(|sink, read_code| crate::ai::run_gemini_manual_flow(sink, read_code)),
        list_models: Box::new(list_models_live),
        connection_test: Box::new(connection_test),
        env_get: Box::new(|name| std::env::var(name).ok()),
    }
}

/// 生产连接测试：构造 `AiClient`，发一条最小请求，错误渲染为
/// `"<HK-… 错误码> <一句话>"`（`AiClientError` 的 Display 不含 secret）。
fn connection_test(config: &AiConfig, credential_path: &Path) -> Result<(), String> {
    let client = crate::ai::AiClient::new(config, credential_path)
        .map_err(|error| format!("{} {}", error.code(), error))?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let context = crate::ai::build_context(
        "Say ok",
        &config.trigger_prefix,
        crate::shell::ShellKind::detect().unwrap_or(crate::shell::ShellKind::Bash),
        &cwd,
        config.send_cwd_basename,
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("HK-AI-NET {error}"))?;
    runtime.block_on(async {
        client
            .request(&context, &tokio_util::sync::CancellationToken::new())
            .await
            .map(|_| ())
            .map_err(|error| format!("{} {}", error.code(), error))
    })
}
