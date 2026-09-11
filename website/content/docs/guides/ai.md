---
title: Explicit AI actions
description: Use Hokan's optional AI provider without giving up review or execution control.
order: 5
keywords:
  - AI completion
  - OpenAI compatible
  - terminal safety
---

AI is optional and explicit. Hokan recognizes natural-language intent locally, and only sends a network request after you select the AI action. The returned command is inserted for review; it is not executed automatically.

This keeps the useful part of AI assistance close to the command line while preserving the same deliberate action model as local suggestions.

## Set up a provider


Run the interactive provider wizard in a real terminal:

```bash
hokan ai setup
```

The wizard uses a colored, step-by-step terminal UI: errors are red, successful
checks are green, and provider/model choices are easy to scan. It tests the
connection before atomically writing the configuration. Set
`ui.color = "never"` or `NO_COLOR=1` when a plain terminal is preferred.

| Route | Providers |
| --- | --- |
| Account sign-in | ChatGPT/Codex, Gemini Code Assist, xAI Grok |
| Direct API | OpenAI, Anthropic, Gemini, xAI, DeepSeek, Groq, Mistral, Together AI, Kimi/Moonshot (global and China), Z.AI/GLM, StepFun, Alibaba/DashScope, Hugging Face, NVIDIA NIM, Xiaomi MiMo, Tencent TokenHub, Fireworks AI, NovitaAI, DeepInfra, MiniMax (global and China) |
| Subscription or gateway | OpenCode Go, OpenCode Zen, OpenRouter, Kimi Coding Plan, Alibaba Coding Plan, Ollama Cloud, Vercel AI Gateway, Kilo Code |
| Local or self-hosted | Ollama, LM Studio, any custom OpenAI-compatible endpoint |

The selected provider determines the wire protocol. Direct Anthropic, MiniMax,
and Kimi Coding Plan use Anthropic Messages, OpenAI API keys use Responses, and
OpenCode Go or Zen switch between Chat Completions, Messages, and Responses
according to the selected model. Other gateways remain on their
OpenAI-compatible route. Kimi/Moonshot chat requests omit `temperature` so the
provider can apply the model's required server-side setting.

For OpenCode Go or Zen, create a key at <https://opencode.ai/auth> and select
the matching route in the wizard. For OpenRouter, create a key at
<https://openrouter.ai/settings/keys> and select `OpenRouter`; its model list
is loaded from the OpenRouter `/models` endpoint when available. Kimi Coding
Plan keys (`sk-kimi-...`) use the dedicated `Kimi Coding Plan` route, while
Moonshot platform keys use `Kimi / Moonshot`.

The wizard also detects the common provider environment variables, including
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GROQ_API_KEY`, `MISTRAL_API_KEY`,
`KIMI_API_KEY`, `KIMI_CODING_API_KEY`, `ZAI_API_KEY`/`GLM_API_KEY`,
`DASHSCOPE_API_KEY`, `OPENCODE_GO_API_KEY`, `HF_TOKEN`, `NOVITA_API_KEY`,
`DEEPINFRA_API_KEY`, and the gateway-specific variables shown during setup.
Accepted values are copied into the private per-provider credential store;
manually entered keys are read with terminal echo disabled and are never
printed.

Nous Portal, Qwen OAuth, MiniMax OAuth, GitHub Copilot, AWS Bedrock, and Google
Vertex AI are not presented as working options yet. They require a dedicated
OAuth exchange, external process, or cloud identity flow; their tokens must not
be pasted as ordinary API keys.

For scripted configuration, keep the API key in an environment variable:

```bash
export OPENAI_API_KEY='...'
hokan config ai --enable \
  --endpoint https://api.openai.com/v1 \
  --model gpt-5-mini \
  --api-key-env OPENAI_API_KEY
```

Credentials can also be piped from a password manager into a private `0600`
credentials file:

```bash
password-manager read openai-key | hokan config ai \
  --enable --model gpt-5-mini --api-key-stdin
```

## Review before running

Press `Esc` to cancel a pending request. Returned commands are inserted into the editable command line. Review the text before running it; high-risk generated commands require a second confirmation.

Local completion remains available without enabling AI. Diagnostic logging is disabled by default. See [configuration](/docs/reference/configuration#diagnostic-logging) for what logs contain.
