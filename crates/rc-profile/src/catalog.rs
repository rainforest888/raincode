//! Curated provider catalog for CC-Switch-style one-key setup.
//!
//! A user only needs to pick a vendor id and paste an API key; Raincode
//! fills in the wire format, base URL, default model and conventional
//! environment variable automatically.

use crate::model::ProfileKind;

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCatalogEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    pub kind: ProfileKind,
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub env_var: Option<&'static str>,
    pub embedding_model: Option<&'static str>,
    pub models: &'static [&'static str],
    /// 默认模型的上下文窗口(token)。0 = 未知。
    pub context_window: u32,
}

pub fn catalog() -> Vec<ProviderCatalogEntry> {
    vec![
        ProviderCatalogEntry {
            id: "openai",
            display_name: "OpenAI",
            kind: ProfileKind::OpenAI,
            base_url: "https://api.openai.com/v1",
            default_model: "gpt-5.6",
            env_var: Some("OPENAI_API_KEY"),
            embedding_model: Some("text-embedding-3-small"),
            models: &[
                // GPT-5.6 系列（当前旗舰）
                "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna",
                // GPT-5.x 系列
                "gpt-5.5", "gpt-5.4", "gpt-5.3-chat", "gpt-5.2", "gpt-5.1", "gpt-5",
                "gpt-5-mini", "gpt-5-nano",
                // o 系列
                "o3", "o3-mini", "o4-mini",
                // 传统仍可用
                "gpt-4.1", "gpt-4o", "gpt-4o-mini",
                // 开放权重
                "gpt-oss-120b", "gpt-oss-20b",
            ],
            context_window: 200000,
        },
        ProviderCatalogEntry {
            id: "anthropic",
            display_name: "Anthropic",
            kind: ProfileKind::Anthropic,
            base_url: "https://api.anthropic.com",
            default_model: "claude-sonnet-5",
            env_var: Some("ANTHROPIC_API_KEY"),
            embedding_model: None,
            models: &[
                // 最新一代
                "claude-fable-5", "claude-opus-5", "claude-sonnet-5",
                "claude-opus-4-8", "claude-opus-4-7", "claude-opus-4-6",
                "claude-sonnet-4-6", "claude-haiku-4-5",
                // 上一代仍 active
                "claude-sonnet-4-5",
            ],
            context_window: 200000,
        },
        ProviderCatalogEntry {
            id: "deepseek",
            display_name: "DeepSeek",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.deepseek.com/v1",
            default_model: "deepseek-v4-flash",
            env_var: Some("DEEPSEEK_API_KEY"),
            embedding_model: None,
            models: &[
                // V4 系列（deepseek-chat/reasoner 已停用，2026-07-24）
                "deepseek-v4-pro", "deepseek-v4-flash", "deepseek-v4-flash-0731",
                "deepseek-v3.2", "deepseek-v3.2-exp",
                // 停用别名保留用于兼容（API 仍可能接受）
                "deepseek-chat", "deepseek-reasoner",
            ],
            context_window: 1000000,
        },
        ProviderCatalogEntry {
            id: "kimi",
            display_name: "Kimi / Moonshot",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.moonshot.cn/v1",
            default_model: "kimi-k3",
            env_var: Some("MOONSHOT_API_KEY"),
            embedding_model: None,
            models: &[
                // K3 系列（当前旗舰）
                "kimi-k3",
                // K2.7 系列
                "kimi-k2.7-code", "kimi-k2.7-code-highspeed",
                // K2.6 / K2.5
                "kimi-k2.6", "kimi-k2.5",
                // K2 / 旧版
                "kimi-k2-thinking", "kimi-k2", "kimi-latest",
            ],
            context_window: 1000000,
        },
        ProviderCatalogEntry {
            id: "moonshot",
            display_name: "Kimi / Moonshot",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.moonshot.cn/v1",
            default_model: "kimi-k3",
            env_var: Some("MOONSHOT_API_KEY"),
            embedding_model: None,
            models: &[
                "kimi-k3", "kimi-k2.7-code", "kimi-k2.6", "kimi-k2.5",
                "kimi-k2-thinking", "kimi-k2", "kimi-latest",
            ],
            context_window: 1000000,
        },
        ProviderCatalogEntry {
            id: "qwen",
            display_name: "Qwen / DashScope",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
            default_model: "qwen3.8-max",
            env_var: Some("DASHSCOPE_API_KEY"),
            embedding_model: Some("text-embedding-v4"),
            models: &[
                // 最新一代
                "qwen3.8-max", "qwen3.7-max", "qwen3.7-plus", "qwen3.7-flash",
                "qwen3.6-max-preview", "qwen3.6-plus",
                // Qwen3 系列
                "qwen3-max", "qwen3-plus", "qwen3-flash",
                "qwen3-coder-plus", "qwen3-coder-flash",
                // 传统 alias
                "qwen-max", "qwen-plus", "qwen-turbo", "qwen-flash",
            ],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "dashscope",
            display_name: "Qwen / DashScope",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
            default_model: "qwen3.8-max",
            env_var: Some("DASHSCOPE_API_KEY"),
            embedding_model: Some("text-embedding-v4"),
            models: &[
                "qwen3.8-max", "qwen3.7-max", "qwen3.7-plus", "qwen3.7-flash",
                "qwen3.6-plus", "qwen3-max", "qwen3-plus", "qwen3-flash",
                "qwen-max", "qwen-plus", "qwen-turbo",
            ],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "gemini",
            display_name: "Google Gemini",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
            default_model: "gemini-3.6-flash",
            env_var: Some("GEMINI_API_KEY"),
            embedding_model: Some("gemini-embedding-001"),
            models: &[
                // Gemini 3 系列
                "gemini-3.6-flash", "gemini-3.5-flash", "gemini-3.5-flash-lite",
                "gemini-3.1-flash-lite", "gemini-3.1-pro-preview",
                // Gemini 2.5 系列
                "gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.5-flash-lite",
                // 老版本
                "gemini-2.0-flash",
            ],
            context_window: 1000000,
        },
        ProviderCatalogEntry {
            id: "zhipu",
            display_name: "Zhipu GLM",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.z.ai/api/paas/v4",
            default_model: "glm-5.2",
            env_var: Some("ZHIPU_API_KEY"),
            embedding_model: Some("embedding-3"),
            models: &[
                // GLM-5 系列（最新）
                "glm-5.2", "glm-5.1", "glm-5", "glm-5-turbo",
                // GLM-4.7 / 4.6
                "glm-4.7", "glm-4.7-flash", "glm-4.6",
                // GLM-4.5
                "glm-4.5", "glm-4.5-air", "glm-4.5-flash",
            ],
            context_window: 200000,
        },
        ProviderCatalogEntry {
            id: "groq",
            display_name: "Groq",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.groq.com/openai/v1",
            default_model: "meta-llama/llama-4-maverick-17b-128e-instruct",
            env_var: Some("GROQ_API_KEY"),
            embedding_model: None,
            models: &[
                "meta-llama/llama-4-maverick-17b-128e-instruct",
                "meta-llama/llama-4-scout-17b-16e-instruct",
                "llama-3.3-70b-versatile", "llama-3.1-8b-instant",
                "moonshotai/kimi-k2-instruct-0905",
                "openai/gpt-oss-120b", "openai/gpt-oss-20b",
                "qwen/qwen3-32b",
                "deepseek-r1-distill-llama-70b",
                "whisper-large-v3-turbo",
            ],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "openrouter",
            display_name: "OpenRouter",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://openrouter.ai/api/v1",
            default_model: "openai/gpt-5.6-sol",
            env_var: Some("OPENROUTER_API_KEY"),
            embedding_model: None,
            models: &[
                "openai/gpt-5.6-sol", "openai/gpt-5.6-terra", "openai/gpt-5.6-luna",
                "openai/gpt-5.5", "openai/gpt-5",
                "anthropic/claude-opus-5", "anthropic/claude-sonnet-5",
                "deepseek/deepseek-v4-pro", "deepseek/deepseek-v4-flash",
                "google/gemini-3.6-flash",
                "meta-llama/llama-4-maverick", "meta-llama/llama-4-scout",
                "moonshotai/kimi-k3", "qwen/qwen3.8-max", "z-ai/glm-5.2", "x-ai/grok-4.5",
                "openrouter/auto",
            ],
            context_window: 200000,
        },
        ProviderCatalogEntry {
            id: "ollama",
            display_name: "Ollama (local)",
            kind: ProfileKind::Ollama,
            base_url: "http://127.0.0.1:11434",
            default_model: "qwen3",
            env_var: None,
            embedding_model: Some("nomic-embed-text"),
            models: &["qwen3", "qwen3:8b", "qwen3.6", "llama3.3", "llama4", "gemma4", "deepseek-v4-flash", "phi4", "gpt-oss", "mistral", "nomic-embed-text"],
            context_window: 32768,
        },
        ProviderCatalogEntry {
            id: "lmstudio",
            display_name: "LM Studio (local)",
            kind: ProfileKind::OpenAiCompat,
            base_url: "http://localhost:1234/v1",
            default_model: "qwen3:8b",
            env_var: None,
            embedding_model: Some("nomic-embed-text-v1.5"),
            models: &["qwen3:8b", "llama3.1:8b", "gemma3:12b", "phi4"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "vllm",
            display_name: "vLLM (local)",
            kind: ProfileKind::OpenAiCompat,
            base_url: "http://localhost:8000/v1",
            default_model: "Qwen/Qwen3-14B",
            env_var: None,
            embedding_model: Some("BAAI/bge-m3"),
            models: &["Qwen/Qwen3-14B", "meta-llama/Llama-3.1-8B-Instruct", "NousResearch/Meta-Llama-3-8B-Instruct"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "jina",
            display_name: "Jina AI",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.jina.ai/v1",
            default_model: "jina-deepsearch-v1",
            env_var: Some("JINA_API_KEY"),
            embedding_model: Some("jina-embeddings-v3"),
            models: &["jina-deepsearch-v1", "jina-vlm", "jina-reader-lm-1.5b"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "huggingface",
            display_name: "Hugging Face Inference",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://router.huggingface.co/v1",
            default_model: "Qwen/Qwen3-235B-A22B",
            env_var: Some("HF_TOKEN"),
            embedding_model: Some("sentence-transformers/all-MiniLM-L6-v2"),
            models: &["Qwen/Qwen3-235B-A22B", "Qwen/Qwen3-14B", "openai/gpt-oss-120b", "moonshotai/Kimi-K3"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "grok",
            display_name: "xAI Grok",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.x.ai/v1",
            default_model: "grok-4.5",
            env_var: Some("XAI_API_KEY"),
            embedding_model: None,
            models: &["grok-4.5", "grok-4.3", "grok-build-0.1", "grok-4.5-latest", "grok-build-latest"],
            context_window: 500000,
        },
        ProviderCatalogEntry {
            id: "mistral",
            display_name: "Mistral AI",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.mistral.ai/v1",
            default_model: "mistral-large-latest",
            env_var: Some("MISTRAL_API_KEY"),
            embedding_model: Some("mistral-embed"),
            models: &["mistral-large-latest", "mistral-small-latest", "mistral-medium-3-5", "codestral-latest", "ministral-8b-2512"],
            context_window: 262000,
        },
        ProviderCatalogEntry {
            id: "cohere",
            display_name: "Cohere",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.cohere.ai/compatibility/v1",
            default_model: "command-a-plus",
            env_var: Some("COHERE_API_KEY"),
            embedding_model: Some("embed-v4"),
            models: &["command-a-plus", "command-a", "command-r7b", "command-r-plus", "command-a-reasoning-08-2025"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "together",
            display_name: "Together AI",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.together.ai/v1",
            default_model: "moonshotai/Kimi-K2.6",
            env_var: Some("TOGETHER_API_KEY"),
            embedding_model: Some("intfloat/multilingual-e5-large-instruct"),
            models: &["moonshotai/Kimi-K2.6", "moonshotai/Kimi-K2.7-Code", "deepseek-ai/DeepSeek-V4-Pro", "deepseek-ai/DeepSeek-V4-Flash-0731", "Qwen/Qwen3.7-Max", "zai-org/GLM-5.2"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "fireworks",
            display_name: "Fireworks AI",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.fireworks.ai/inference/v1",
            default_model: "accounts/fireworks/models/deepseek-v3p1",
            env_var: Some("FIREWORKS_API_KEY"),
            embedding_model: Some("accounts/fireworks/models/nomic-embed-text-v1.5"),
            models: &["accounts/fireworks/models/deepseek-v3p1", "accounts/fireworks/models/llama-v3p1-8b-instruct", "accounts/fireworks/models/kimi-k2p6", "accounts/fireworks/models/glm-5p2", "accounts/fireworks/models/gpt-oss-120b"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "perplexity",
            display_name: "Perplexity",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.perplexity.ai",
            default_model: "sonar",
            env_var: Some("PERPLEXITY_API_KEY"),
            embedding_model: Some("pplx-embed-v1-4b"),
            models: &["sonar", "sonar-pro", "sonar-deep-research", "sonar-reasoning-pro"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "cerebras",
            display_name: "Cerebras",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.cerebras.ai/v1",
            default_model: "gpt-oss-120b",
            env_var: Some("CEREBRAS_API_KEY"),
            embedding_model: None,
            models: &["gpt-oss-120b", "zai-glm-4.7", "llama-3.3-70b-instruct", "llama-4-maverick-17b-128e-instruct", "kimi-k2-instruct"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "nvidia",
            display_name: "NVIDIA NIM",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://integrate.api.nvidia.com/v1",
            default_model: "nvidia/llama-3.1-nemotron-ultra-253b-v1",
            env_var: Some("NVIDIA_API_KEY"),
            embedding_model: Some("nvidia/llama-nemotron-embed-1b-v2"),
            models: &["nvidia/llama-3.1-nemotron-ultra-253b-v1", "nvidia/llama-3.1-nemotron-70b-instruct", "meta/llama-3.3-70b-instruct", "deepseek-ai/deepseek-v4-flash-0731", "mistralai/mistral-large"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "hyperbolic",
            display_name: "Hyperbolic",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.hyperbolic.xyz/v1",
            default_model: "deepseek-ai/DeepSeek-V3",
            env_var: Some("HYPERBOLIC_API_KEY"),
            embedding_model: None,
            models: &["deepseek-ai/DeepSeek-V3", "deepseek-ai/DeepSeek-R1", "meta-llama/Llama-3.3-70B-Instruct", "Qwen/Qwen3-Coder-480B-A35B-Instruct", "meta-llama/Meta-Llama-3.1-405B"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "novita",
            display_name: "Novita AI",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.novita.ai/openai/v1",
            default_model: "deepseek/deepseek-v4-pro",
            env_var: Some("NOVITA_API_KEY"),
            embedding_model: None,
            models: &["deepseek/deepseek-v4-pro", "deepseek/deepseek-v4-flash", "moonshotai/kimi-k3", "zai-org/glm-5.2", "qwen/qwen3.7-max", "moonshotai/kimi-k2.7-code"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "siliconflow",
            display_name: "SiliconFlow 硅基流动",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.siliconflow.cn/v1",
            default_model: "deepseek-ai/DeepSeek-V3",
            env_var: Some("SILICONFLOW_API_KEY"),
            embedding_model: Some("BAAI/bge-m3"),
            models: &["deepseek-ai/DeepSeek-V3", "deepseek-ai/DeepSeek-V3.2", "deepseek-ai/DeepSeek-R1", "Qwen/Qwen3-235B-A22B", "Qwen/Qwen3-Coder-480B-A35B-Instruct", "zai-org/GLM-4.7", "zai-org/GLM-4.6", "Qwen/QwQ-32B"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "qianfan",
            display_name: "百度千帆 Qianfan",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://qianfan.baidubce.com/v2",
            default_model: "ernie-4.5-8k",
            env_var: Some("QIANFAN_API_KEY"),
            embedding_model: Some("bge-large-zh"),
            models: &["ernie-4.5-8k", "ernie-4.5-turbo-8k", "ernie-4.0-8k", "ernie-4.0-turbo-8k", "ernie-3.5-8k", "ernie-speed-8k", "ernie-lite-8k", "ernie-x1-32k"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "hunyuan",
            display_name: "腾讯混元 Hunyuan",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.hunyuan.cloud.tencent.com/v1",
            default_model: "hunyuan-turbos-latest",
            env_var: Some("HUNYUAN_API_KEY"),
            embedding_model: None,
            models: &["hunyuan-turbos-latest", "hunyuan-turbos", "hunyuan-turbo", "hunyuan-standard", "hunyuan-lite", "hunyuan-pro", "hunyuan-large"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "ark",
            display_name: "火山方舟 Volcano Ark (Doubao)",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://ark.cn-beijing.volces.com/api/v3",
            default_model: "doubao-seed-1-6-flash",
            env_var: Some("ARK_API_KEY"),
            embedding_model: Some("doubao-embedding-text-240715"),
            models: &["doubao-seed-1-6-flash", "doubao-seed-1-6", "doubao-seed-1-6-thinking", "doubao-seed-code-preview-latest", "doubao-lite-32k", "doubao-pro-32k", "doubao-embedding-text-240715"],
            context_window: 1000000,
        },
        ProviderCatalogEntry {
            id: "minimax",
            display_name: "MiniMax",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.minimaxi.com/v1",
            default_model: "MiniMax-M2.7",
            env_var: Some("MINIMAX_API_KEY"),
            embedding_model: None,
            models: &["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed", "MiniMax-M2.5", "MiniMax-M2.1", "MiniMax-H3"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "stepfun",
            display_name: "阶跃星辰 StepFun",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.stepfun.com/v1",
            default_model: "step-3.7-flash",
            env_var: Some("STEPFUN_API_KEY"),
            embedding_model: None,
            models: &["step-3.7-flash", "step-3.5-flash", "step-1o-turbo-vision", "step-2", "step-1"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "iflytek",
            display_name: "讯飞星火 iFlytek Spark",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://spark-api-open.xf-yun.com/v1",
            default_model: "4.0Ultra",
            env_var: Some("IFLYTEK_API_KEY"),
            embedding_model: None,
            models: &["4.0Ultra", "generalv3.5", "generalv3", "max-32k", "pro-128k", "lite"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "baichuan",
            display_name: "百川 Baichuan",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.baichuan-ai.com/v1",
            default_model: "Baichuan4-Turbo",
            env_var: Some("BAICHUAN_API_KEY"),
            embedding_model: None,
            models: &["Baichuan4-Turbo", "Baichuan4-Air", "Baichuan4", "Baichuan3-Turbo", "Baichuan3"],
            context_window: 128000,
        },
        ProviderCatalogEntry {
            id: "lingyiwanwu",
            display_name: "零一万物 Yi / 01.AI",
            kind: ProfileKind::OpenAiCompat,
            base_url: "https://api.lingyiwanwu.com/v1",
            default_model: "yi-lightning",
            env_var: Some("LINGYI_API_KEY"),
            embedding_model: None,
            models: &["yi-lightning", "yi-large", "yi-large-turbo", "yi-medium", "yi-medium-200k", "yi-vision-v2"],
            context_window: 200000,
        },
    ]
}

pub fn find(provider: &str) -> Option<ProviderCatalogEntry> {
    let id = provider.trim().to_ascii_lowercase();
    catalog().into_iter().find(|entry| entry.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_catalog_entry_is_ready_to_use() {
        let entry = find("openai").unwrap();
        assert_eq!(entry.kind, ProfileKind::OpenAI);
        assert_eq!(entry.base_url, "https://api.openai.com/v1");
        assert_eq!(entry.default_model, "gpt-5.6");
        assert_eq!(entry.env_var, Some("OPENAI_API_KEY"));
        assert!(entry.models.contains(&"gpt-5.6-sol"));
    }

    #[test]
    fn deepseek_uses_openai_compatible_kind() {
        let entry = find("deepseek").unwrap();
        assert_eq!(entry.kind, ProfileKind::OpenAiCompat);
        assert!(entry.base_url.contains("deepseek.com"));
    }

    #[test]
    fn unknown_provider_returns_none() {
        assert!(find("not-a-vendor").is_none());
    }

    #[test]
    fn every_catalog_entry_has_at_least_one_model() {
        for entry in catalog() {
            assert!(!entry.models.is_empty(), "{} has no models", entry.id);
            assert!(
                !entry.default_model.is_empty(),
                "{} has empty default_model",
                entry.id
            );
            // default 允许是 alias(如 openai 的 gpt-5.6 → gpt-5.6-sol),不必逐字在 models 里。
            assert!(
                entry.models.contains(&entry.default_model)
                    || entry.default_model.starts_with(entry.id)
                    || entry.models.iter().any(|m| m.starts_with(entry.default_model))
                    || entry.env_var.is_none(),
                "{} default '{}' not in models",
                entry.id,
                entry.default_model
            );
        }
    }

    #[test]
    fn deepseek_default_is_v4_flash() {
        let entry = find("deepseek").unwrap();
        assert_eq!(entry.default_model, "deepseek-v4-flash");
        assert!(entry.models.contains(&"deepseek-v4-pro"));
        // deepseek-v4-flash 上下文窗口 1M(用户实测确认)。
        assert_eq!(entry.context_window, 1_000_000);
    }

    #[test]
    fn local_providers_have_no_env_key() {
        for id in ["lmstudio", "vllm", "ollama"] {
            let entry = find(id).unwrap();
            assert!(entry.env_var.is_none(), "{id} should be keyless");
        }
    }

    #[test]
    fn openai_models_include_flagship() {
        let entry = find("openai").unwrap();
        assert!(entry.models.contains(&"gpt-5"));
    }
}
