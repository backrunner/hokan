//! 在线模型列表：按服务商拉取并用宽松解析器兼容多种响应形状。

use std::time::Duration;

use super::ModelListQuery;

/// 在线拉取模型列表的总预算。
const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(3);

/// 模型列表响应体的大小上限；超限视为拉取失败（调用方回退静态表）。
pub(super) const MODEL_LIST_MAX_BYTES: usize = 1024 * 1024;

/// 在线拉取模型列表；任何失败都返回 `None` 由调用方回退到静态表。
pub(super) fn list_models_live(query: &ModelListQuery<'_>) -> Option<Vec<String>> {
    let url = match query.slug {
        // Gemini OAuth 没有可用的 OpenAI 兼容列表端点，直接用静态表。
        "gemini-oauth" => return None,
        "ollama" => {
            let base = query.endpoint.trim_end_matches('/');
            let base = base.strip_suffix("/v1").unwrap_or(base);
            format!("{base}/api/tags")
        }
        "openai-oauth" => format!(
            "{}/models?client_version=1.0.0",
            query.endpoint.trim_end_matches('/')
        ),
        _ => format!("{}/models", query.endpoint.trim_end_matches('/')),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let client = reqwest::Client::builder()
        .connect_timeout(MODEL_LIST_TIMEOUT)
        .timeout(MODEL_LIST_TIMEOUT)
        // 携带凭据的请求绝不跟随重定向。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let bearer = query.bearer.map(str::to_owned);
    let account_id = query.account_id.map(str::to_owned);
    runtime.block_on(async move {
        let mut request = client.get(url);
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        if let Some(account_id) = account_id {
            request = request.header("ChatGPT-Account-Id", account_id);
        }
        let mut response = request.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        // Content-Length 可能缺失或谎报，流式累积时仍逐块兜底上限。
        if response
            .content_length()
            .is_some_and(|length| length > MODEL_LIST_MAX_BYTES as u64)
        {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len() + chunk.len() > MODEL_LIST_MAX_BYTES {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        parse_model_names(&body)
    })
}

/// 兼容三种列表响应形状：OpenAI `data[].id`、Ollama `models[].name`、
/// Codex `models[].slug`。
pub(super) fn parse_model_names(body: &[u8]) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    for key in ["data", "models"] {
        let Some(items) = value.get(key).and_then(|array| array.as_array()) else {
            continue;
        };
        let names: Vec<String> = items
            .iter()
            .filter_map(|item| {
                item.get("id")
                    .or_else(|| item.get("slug"))
                    .or_else(|| item.get("name"))
                    .and_then(|name| name.as_str())
                    .map(str::to_owned)
            })
            .collect();
        if !names.is_empty() {
            return Some(names);
        }
    }
    None
}
