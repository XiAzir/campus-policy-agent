use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    // 主模型（Gemini 原生 /v1beta 反代）
    pub gemini_base_url: String,
    pub gemini_api_key: String,
    pub gemini_model: String,

    // Embedding（硅基流动，查询侧；资料包内已带向量）
    pub siliconflow_base_url: String,
    pub siliconflow_api_key: String,
    pub siliconflow_model: String,
    pub embed_dims: usize,
    pub preprocessing_version: String,

    // 数据目录与前端目录
    pub data_dir: PathBuf,
    pub frontend_dist: Option<PathBuf>,

    // 访问控制
    pub token_ttl_days: i64,
    pub initial_admin_password: String,

    // 问答并发与排队
    pub chat_concurrency: usize,
    pub chat_queue_max: usize,
    pub chat_history_max_rounds: usize,
    pub chat_history_max_chars: usize,
    pub chat_request_timeout_s: u64,
    pub chat_max_tool_rounds: usize,

    // 资料包与磁盘
    pub max_package_mb: usize,
    pub disk_warn_gb: f64,
    pub max_upload_body_mb: usize,

    // 运行
    pub host: String,
    pub port: u16,
}

fn get_env_or(key: &str, default_val: &str) -> String {
    env::var(key).unwrap_or_else(|_| default_val.to_string())
}

fn get_env_int<T: std::str::FromStr>(key: &str, default_val: T) -> T {
    env::var(key)
        .ok()
        .and_then(|val| val.parse::<T>().ok())
        .unwrap_or(default_val)
}

fn get_env_float(key: &str, default_val: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|val| val.parse::<f64>().ok())
        .unwrap_or(default_val)
}

impl Config {
    pub fn from_env(base_dir: Option<&Path>) -> Self {
        let fallback_data_dir = match base_dir {
            Some(p) => p.join("backend").join("data"),
            None => PathBuf::from("backend/data"),
        };

        let data_dir_str = env::var("DATA_DIR").unwrap_or_default();
        let data_dir = if data_dir_str.is_empty() {
            fallback_data_dir
        } else {
            PathBuf::from(data_dir_str)
        };

        let frontend_dist_str = env::var("FRONTEND_DIST").unwrap_or_default();
        let frontend_dist = if frontend_dist_str.is_empty() {
            base_dir.map(|p| p.join("frontend").join("dist"))
        } else {
            Some(PathBuf::from(frontend_dist_str))
        };

        let chat_concurrency = get_env_int("CHAT_CONCURRENCY", 1);
        if chat_concurrency != 1 {
            panic!(
                "CHAT_CONCURRENCY 首版仅支持 1（当前设置: {}），避免歧义配置启动",
                chat_concurrency
            );
        }

        Self {
            gemini_base_url: get_env_or("GEMINI_BASE_URL", "")
                .trim_end_matches('/')
                .to_string(),
            gemini_api_key: get_env_or("GEMINI_API_KEY", ""),
            gemini_model: get_env_or("GEMINI_MODEL", ""),

            siliconflow_base_url: get_env_or(
                "SILICONFLOW_BASE_URL",
                "https://api.siliconflow.cn/v1",
            )
            .trim_end_matches('/')
            .to_string(),
            siliconflow_api_key: get_env_or("SILICONFLOW_API_KEY", ""),
            siliconflow_model: get_env_or("SILICONFLOW_EMBED_MODEL", ""),
            embed_dims: get_env_int("EMBED_DIMS", 1024),
            preprocessing_version: get_env_or("PREPROCESSING_VERSION", "v1"),

            data_dir,
            frontend_dist,

            token_ttl_days: get_env_int("TOKEN_TTL_DAYS", 30),
            initial_admin_password: get_env_or("INITIAL_ADMIN_PASSWORD", "admin"),

            chat_concurrency,
            chat_queue_max: get_env_int("CHAT_QUEUE_MAX", 10),
            chat_history_max_rounds: get_env_int("CHAT_HISTORY_MAX_ROUNDS", 12),
            chat_history_max_chars: get_env_int("CHAT_HISTORY_MAX_CHARS", 8000),
            chat_request_timeout_s: get_env_int("CHAT_REQUEST_TIMEOUT_S", 180),
            chat_max_tool_rounds: 3,

            max_package_mb: get_env_int("MAX_PACKAGE_MB", 200),
            disk_warn_gb: get_env_float("DISK_WARN_GB", 8.0),
            max_upload_body_mb: get_env_int("MAX_UPLOAD_BODY_MB", 210),

            host: get_env_or("HOST", "127.0.0.1"),
            port: get_env_int("PORT", 8000),
        }
    }

    pub fn ensure_secrets(&self) -> Result<(), String> {
        let mut missing = Vec::new();
        if self.gemini_base_url.is_empty() {
            missing.push("GEMINI_BASE_URL");
        }
        if self.gemini_api_key.is_empty() {
            missing.push("GEMINI_API_KEY");
        }
        if self.gemini_model.is_empty() {
            missing.push("GEMINI_MODEL");
        }
        if !missing.is_empty() {
            return Err(format!(
                "缺少主模型配置：{}（应在环境中配置）",
                missing.join(", ")
            ));
        }
        if self.siliconflow_api_key.is_empty() {
            return Err("缺少 SILICONFLOW_API_KEY（查询侧向量化需要）".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;

    #[test]
    fn test_config_matches_contract_fixture() {
        // 读取 M0 导出的 config_defaults.json 夹具
        let fixture_path = Path::new("tests/fixtures/config/config_defaults.json");
        let content = fs::read_to_string(fixture_path).expect("读取配置默认值夹具失败");
        let fixture: Value = serde_json::from_str(&content).expect("解析夹具 JSON 失败");

        // 在无环境变量覆盖的情况下构造默认 Config
        let cfg = Config::from_env(None);

        assert_eq!(
            cfg.token_ttl_days,
            fixture["token_ttl_days"].as_i64().unwrap()
        );
        assert_eq!(
            cfg.chat_concurrency,
            fixture["chat_concurrency"].as_u64().unwrap() as usize
        );
        assert_eq!(
            cfg.chat_queue_max,
            fixture["chat_queue_max"].as_u64().unwrap() as usize
        );
        assert_eq!(
            cfg.chat_history_max_rounds,
            fixture["chat_history_max_rounds"].as_u64().unwrap() as usize
        );
        assert_eq!(
            cfg.chat_history_max_chars,
            fixture["chat_history_max_chars"].as_u64().unwrap() as usize
        );
        assert_eq!(
            cfg.chat_request_timeout_s,
            fixture["chat_request_timeout_s"].as_u64().unwrap()
        );
        assert_eq!(
            cfg.chat_max_tool_rounds,
            fixture["chat_max_tool_rounds"].as_u64().unwrap() as usize
        );
        assert_eq!(
            cfg.max_package_mb,
            fixture["max_package_mb"].as_u64().unwrap() as usize
        );
        assert_eq!(cfg.disk_warn_gb, fixture["disk_warn_gb"].as_f64().unwrap());
        assert_eq!(
            cfg.max_upload_body_mb,
            fixture["max_upload_body_mb"].as_u64().unwrap() as usize
        );
        assert_eq!(
            cfg.embed_dims,
            fixture["embed_dims"].as_u64().unwrap() as usize
        );
        assert_eq!(
            cfg.preprocessing_version,
            fixture["preprocessing_version"].as_str().unwrap()
        );
        assert_eq!(cfg.host, fixture["host"].as_str().unwrap());
        assert_eq!(cfg.port, fixture["port"].as_u64().unwrap() as u16);
        assert_eq!(
            cfg.initial_admin_password,
            fixture["initial_admin_password"].as_str().unwrap()
        );
    }

    #[test]
    fn test_ensure_secrets() {
        let mut cfg = Config::from_env(None);
        assert!(cfg.ensure_secrets().is_err());

        cfg.gemini_base_url = "https://gemini.example.com".to_string();
        cfg.gemini_api_key = "secret_key".to_string();
        cfg.gemini_model = "gemini-2.5-flash".to_string();
        cfg.siliconflow_api_key = "silicon_key".to_string();

        assert!(cfg.ensure_secrets().is_ok());
    }
}
