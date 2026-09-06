use jieba_rs::Jieba;
use regex::Regex;
use std::sync::LazyLock;

static JIEBA: LazyLock<Jieba> = LazyLock::new(Jieba::new);

// Python 基线正则: r'^[^\s"\'()*:{}]+$'
static FT_SAFE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^[^\s"'()*:{}]+$"#).expect("编译 FTS 安全词正则失败")
});

/// 针对 FTS 写入的分词：按 jieba 搜索模式切词、去除空白后用单个空格连接
pub fn tokenize_for_fts(text: &str) -> String {
    let words = JIEBA.cut_for_search(text, true);
    let filtered: Vec<&str> = words
        .into_iter()
        .map(|w| w.trim())
        .filter(|w| !w.is_empty())
        .collect();
    filtered.join(" ")
}

/// 针对 FTS 查询的 MATCH 构造：搜索模式切词、去重、过滤非法符号、限制最多 max_tokens 个、用双引号包裹、OR 连接
pub fn fts_match_query(text: &str, max_tokens: usize) -> String {
    let words = JIEBA.cut_for_search(text, true);
    let mut tokens: Vec<String> = Vec::new();

    for w in words {
        let t = w.trim();
        if !t.is_empty() && FT_SAFE.is_match(t) && !tokens.iter().any(|existing| existing == t) {
            tokens.push(t.to_string());
        }
        if tokens.len() >= max_tokens {
            break;
        }
    }

    if tokens.is_empty() {
        return String::new();
    }

    tokens
        .into_iter()
        .map(|t| format!("\"{}\"", t))
        .collect::<Vec<String>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::fs;
    use std::path::Path;

    #[test]
    fn test_jieba_golden_alignment() {
        let fixture_path = Path::new("tests/fixtures/tokenize/jieba_golden.json");
        let content = fs::read_to_string(fixture_path).expect("读取分词夹具失败");
        let fixture: Value = serde_json::from_str(&content).unwrap();

        // 1. 测试语料库分词结果与 FTS 构造
        for item in fixture["corpus"].as_array().unwrap() {
            let text = item["text"].as_str().unwrap();
            let expected_fts_body = item["fts_index_body"].as_str().unwrap();
            let expected_match = item["fts_match_query"].as_str().unwrap();
            let expected_match_max8 = item["fts_match_query_max8"].as_str().unwrap();

            let actual_fts_body = tokenize_for_fts(text);
            assert_eq!(
                actual_fts_body, expected_fts_body,
                "fts_index_body 与 Python 不一致: text={}",
                text
            );

            let actual_match = fts_match_query(text, 24);
            assert_eq!(
                actual_match, expected_match,
                "fts_match_query (max 24) 与 Python 不一致: text={}",
                text
            );

            let actual_match_max8 = fts_match_query(text, 8);
            assert_eq!(
                actual_match_max8, expected_match_max8,
                "fts_match_query (max 8) 与 Python 不一致: text={}",
                text
            );
        }

        // 2. 测试查询用例
        for q in fixture["queries"].as_array().unwrap() {
            let text = q["text"].as_str().unwrap();
            let expected_match = q["match"].as_str().unwrap();

            let actual_match = fts_match_query(text, 24);
            assert_eq!(
                actual_match, expected_match,
                "query MATCH 表达式不一致: text={}",
                text
            );
        }
    }
}
