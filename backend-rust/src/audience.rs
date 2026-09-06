use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;

static RE_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4})(?:级|年入学)$").expect("编译年份正则失败")
});

static GENERAL_LABELS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut s = HashSet::new();
    s.insert("全校");
    s.insert("全校学生");
    s.insert("全体学生");
    s.insert("不限");
    s
});

/// 从 audience 与 audience_scope 提取约束：(colleges, entry_years, unknown)
pub fn extract_constraints(
    audience: &Value,
    audience_scope: &Value,
) -> (Vec<String>, Vec<String>, bool) {
    // 1. 若 audience_scope 含有 confirmed == true，以 scope 为准
    if let Some(scope_obj) = audience_scope.as_object() {
        if scope_obj.get("confirmed").and_then(|v| v.as_bool()).unwrap_or(false) {
            let colleges = scope_obj
                .get("colleges")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let entry_years = scope_obj
                .get("entry_years")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            return (colleges, entry_years, false);
        }
    }

    // 2. 从 audience 标签数组中提取
    let mut colleges = Vec::new();
    let mut years = Vec::new();
    let labels = audience.as_array();
    let unknown = labels.map(|arr| arr.is_empty()).unwrap_or(true);

    if let Some(arr) = labels {
        for item in arr {
            if let Some(label) = item.as_str() {
                if GENERAL_LABELS.contains(label) {
                    continue;
                }
                if label.ends_with("学院") {
                    colleges.push(label.to_string());
                } else if let Some(caps) = RE_YEAR.captures(label) {
                    if let Some(m) = caps.get(1) {
                        years.push(m.as_str().to_string());
                    }
                }
            }
        }
    }

    (colleges, years, unknown)
}

/// 判定学生画像是否符合受众要求：(is_match, missing_labels)
pub fn match_audience(
    audience: &Value,
    audience_scope: &Value,
    profile_college: &str,
    profile_entry_year: &str,
) -> (bool, Vec<String>) {
    let (colleges, years, unknown) = extract_constraints(audience, audience_scope);
    let mut missing = Vec::new();

    if unknown {
        missing.push("资料适用范围待管理员确认".to_string());
    }

    let college_clean = profile_college.trim();
    if !colleges.is_empty() {
        if college_clean.is_empty() {
            missing.push("学院".to_string());
        } else if !colleges.iter().any(|c| c == college_clean) {
            return (false, Vec::new());
        }
    }

    let year_clean = profile_entry_year.trim();
    if !years.is_empty() {
        if year_clean.is_empty() {
            missing.push("入学年份".to_string());
        } else if !years.iter().any(|y| y == year_clean) {
            return (false, Vec::new());
        }
    }

    (missing.is_empty(), missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_general_audience_matches_all() {
        let aud = json!(["全校"]);
        let scope = json!({});

        let (matched, missing) = match_audience(&aud, &scope, "", "");
        assert!(matched);
        assert!(missing.is_empty());

        let (matched, missing) = match_audience(&aud, &scope, "计算机学院", "2025");
        assert!(matched);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_college_and_year_filter() {
        let aud = json!(["信息工程学院", "2025级"]);
        let scope = json!({});

        // 缺少画像信息
        let (matched, missing) = match_audience(&aud, &scope, "", "");
        assert!(!matched);
        assert_eq!(missing, vec!["学院", "入学年份"]);

        // 学院不匹配
        let (matched, _) = match_audience(&aud, &scope, "文学院", "2025");
        assert!(!matched);

        // 完全匹配
        let (matched, missing) = match_audience(&aud, &scope, "信息工程学院", "2025");
        assert!(matched);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_confirmed_scope_override() {
        let aud = json!(["全校"]); // 标签是全校，但 scope 显式 confirmed 限定计算机学院
        let scope = json!({
            "confirmed": true,
            "colleges": ["计算机学院"],
            "entry_years": ["2024"]
        });

        let (matched, _) = match_audience(&aud, &scope, "文学院", "2024");
        assert!(!matched);

        let (matched, missing) = match_audience(&aud, &scope, "计算机学院", "2024");
        assert!(matched);
        assert!(missing.is_empty());
    }
}
