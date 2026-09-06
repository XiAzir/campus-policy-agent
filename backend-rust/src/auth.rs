use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

pub const PBKDF2_ITERATIONS: u32 = 200_000;
pub const SALT_LEN: usize = 16;
pub const DIGEST_LEN: usize = 32;

/// 口令哈希：PBKDF2-HMAC-SHA256, 200,000 次, 16 字节 salt, 32 字节输出
/// 格式: pbkdf2$<salt_hex>$<digest_hex>
pub fn hash_password(password: &str, salt: Option<&[u8]>) -> String {
    let mut random_salt = [0u8; SALT_LEN];
    let actual_salt = match salt {
        Some(s) => s,
        None => {
            rand::thread_rng().fill_bytes(&mut random_salt);
            &random_salt
        }
    };
    let mut output = [0u8; DIGEST_LEN];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), actual_salt, PBKDF2_ITERATIONS, &mut output);
    format!("pbkdf2${}${}", hex::encode(actual_salt), hex::encode(output))
}

/// 验证口令，常量时间比对
pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 3 || parts[0] != "pbkdf2" {
        return false;
    }
    let Ok(salt) = hex::decode(parts[1]) else {
        return false;
    };
    let Ok(expected_digest) = hex::decode(parts[2]) else {
        return false;
    };
    if salt.len() != SALT_LEN || expected_digest.len() != DIGEST_LEN {
        return false;
    }
    let mut output = [0u8; DIGEST_LEN];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, PBKDF2_ITERATIONS, &mut output);
    output.ct_eq(&expected_digest).into()
}

/// 令牌负载，字段 r (role), c (client_id), e (exp 秒级时间戳)
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct TokenPayload {
    pub r: String,
    pub c: String,
    pub e: i64,
}

#[derive(Clone)]
pub struct TokenService {
    key: Vec<u8>,
    ttl_days: i64,
}

impl TokenService {
    pub fn from_hex_secret(secret_hex: &str, ttl_days: i64) -> Result<Self, String> {
        let key = hex::decode(secret_hex).map_err(|e| format!("token_secret 并非合法 hex: {}", e))?;
        Ok(Self { key, ttl_days })
    }

    /// 签发令牌：base64url(payload) + '.' + 前32字符 hex(HMAC-SHA256(raw))
    pub fn issue(&self, role: &str, client_id: &str) -> String {
        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let exp = now_sec + self.ttl_days * 86400;

        let payload = TokenPayload {
            r: role.to_string(),
            c: client_id.to_string(),
            e: exp,
        };

        let json_bytes = serde_json::to_vec(&payload).expect("序列化 TokenPayload 失败");
        let raw = URL_SAFE_NO_PAD.encode(&json_bytes);

        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC 初始化失败");
        mac.update(raw.as_bytes());
        let full_sig_hex = hex::encode(mac.finalize().into_bytes());
        let sig = &full_sig_hex[..32];

        format!("{}.{}", raw, sig)
    }

    /// 验签与过期检查，支持直接解析任意合法 base64url payload（不重序列化）
    pub fn verify(&self, token: &str, expected_role: &str) -> Option<String> {
        let (raw, sig) = token.rsplit_once('.')?;
        if sig.len() != 32 {
            return None;
        }

        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC 初始化失败");
        mac.update(raw.as_bytes());
        let full_sig_hex = hex::encode(mac.finalize().into_bytes());
        let expected_sig = &full_sig_hex[..32];

        // 常量时间比对前 32 字符 hex 签名
        if !bool::from(sig.as_bytes().ct_eq(expected_sig.as_bytes())) {
            return None;
        }

        // 解码 payload（使用 URL_SAFE 支持可选 padding）
        let payload_bytes = URL_SAFE_NO_PAD.decode(raw).or_else(|_| {
            // 兼容可能带 padding 的情况
            base64::engine::general_purpose::URL_SAFE.decode(raw)
        }).ok()?;

        let payload: TokenPayload = serde_json::from_slice(&payload_bytes).ok()?;

        if payload.r != expected_role {
            return None;
        }

        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        if payload.e < now_sec {
            return None;
        }

        Some(payload.c)
    }
}

/// 内存令牌桶限流器（Spec 5.1 & 6.2）：上限 10,000 桶，饱和时不释放活跃桶
pub struct RateLimiter {
    // key -> (tokens, last_time)
    buckets: Mutex<std::collections::HashMap<String, (f64, Instant)>>,
    max_keys: usize,
}

impl RateLimiter {
    pub fn new(max_keys: usize) -> Self {
        Self {
            buckets: Mutex::new(std::collections::HashMap::new()),
            max_keys,
        }
    }

    /// 尝试消耗 cost 个令牌。若不足或桶饱和返回 false，充足扣除并返回 true
    pub fn hit(&self, key: &str, rate_per_min: f64, burst: f64, cost: f64) -> bool {
        let mut guard = self.buckets.lock().unwrap();
        let now = Instant::now();

        // 过期被动清理（如果超过阈值且该 key 不存在）
        if !guard.contains_key(key) && guard.len() >= self.max_keys {
            // 清理超过 10 分钟未更新的 key
            guard.retain(|_, (_, last)| now.duration_since(*last) < Duration::from_secs(600));
            if guard.len() >= self.max_keys {
                // 仍旧饱和，保守拒绝
                return false;
            }
        }

        let (tokens, last) = guard.get(key).copied().unwrap_or((burst, now));
        let elapsed_s = now.duration_since(last).as_secs_f64();
        let replenished = (tokens + elapsed_s * (rate_per_min / 60.0)).min(burst);

        if replenished < cost {
            guard.insert(key.to_string(), (replenished, now));
            false
        } else {
            guard.insert(key.to_string(), (replenished - cost, now));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::path::Path;

    #[test]
    fn test_password_vectors_fixture() {
        let fixture_path = Path::new("tests/fixtures/auth/password_vectors.json");
        let content = fs::read_to_string(fixture_path).expect("读取口令夹具失败");
        let fixture: Value = serde_json::from_str(&content).expect("解析夹具 JSON 失败");

        for case in fixture["cases"].as_array().unwrap() {
            let pwd = case["password"].as_str().unwrap();
            let salt_hex = case["salt_hex"].as_str().unwrap();
            let expected_stored = case["stored"].as_str().unwrap();
            let salt = hex::decode(salt_hex).unwrap();

            // 生成测试
            let generated = hash_password(pwd, Some(&salt));
            assert_eq!(generated, expected_stored, "口令生成 hash 不匹配: {}", pwd);

            // 验证测试
            assert!(verify_password(pwd, expected_stored), "口令验证应成功: {}", pwd);
            assert!(!verify_password(&format!("{}_wrong", pwd), expected_stored), "错误口令应失败: {}", pwd);
        }

        for mal in fixture["malformed"].as_array().unwrap() {
            let mal_str = mal.as_str().unwrap();
            assert!(!verify_password("any", mal_str), "畸形哈希应拒绝: {}", mal_str);
        }
    }

    #[test]
    fn test_token_vectors_fixture() {
        let fixture_path = Path::new("tests/fixtures/auth/token_vectors.json");
        let content = fs::read_to_string(fixture_path).expect("读取令牌夹具失败");
        let fixture: Value = serde_json::from_str(&content).expect("解析夹具 JSON 失败");

        let secret_hex = fixture["secret_hex"].as_str().unwrap();
        let token_service = TokenService::from_hex_secret(secret_hex, 30).unwrap();

        for case in fixture["cases"].as_array().unwrap() {
            let token = case["token"].as_str().unwrap();
            let expected_role = case["role"].as_str().unwrap();
            let expected_cid = case["client_id"].as_str().unwrap();
            let expired = case["expired"].as_bool().unwrap();

            let verified = token_service.verify(token, expected_role);
            if expired {
                assert!(verified.is_none(), "过期令牌应被拒绝: {}", token);
            } else {
                assert_eq!(verified.as_deref(), Some(expected_cid), "未过期令牌验签 client_id 应一致");
                // 角色不匹配应拒绝
                let wrong_role = if expected_role == "user" { "admin" } else { "user" };
                assert!(token_service.verify(token, wrong_role).is_none(), "错误角色应被拒绝");
            }
        }

        // 被篡改的令牌
        let tampered = fixture["tampered_token"].as_str().unwrap();
        assert!(token_service.verify(tampered, "user").is_none(), "篡改令牌应被拒绝");
    }

    #[test]
    fn test_rate_limiter() {
        let limiter = RateLimiter::new(10);
        // 允许 burst = 2
        assert!(limiter.hit("ip:127.0.0.1", 60.0, 2.0, 1.0));
        assert!(limiter.hit("ip:127.0.0.1", 60.0, 2.0, 1.0));
        // 第 3 次超限
        assert!(!limiter.hit("ip:127.0.0.1", 60.0, 2.0, 1.0));
    }
}
