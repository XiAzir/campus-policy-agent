"""Conservative matching of explicit college and entry-year constraints."""

import json
import re

GENERAL = {"全校", "全校学生", "全体学生", "不限"}


def validate_scope(value):
    if not isinstance(value, dict) or set(value) - {"colleges", "entry_years", "confirmed"}:
        raise ValueError("适用范围结构不合法")
    for key in ("colleges", "entry_years"):
        items = value.get(key, [])
        if not isinstance(items, list) or len(items) > 100 or not all(isinstance(x, str) and 0 < len(x) <= 60 for x in items):
            raise ValueError("适用范围必须是有界字符串数组")
    if any(not re.fullmatch(r"\d{4}", x) for x in value.get("entry_years", [])):
        raise ValueError("适用入学年份必须是四位年份")
    if "confirmed" in value and not isinstance(value["confirmed"], bool):
        raise ValueError("适用范围确认状态不合法")


def constraints(doc):
    raw = doc["audience_scope"] if "audience_scope" in doc.keys() else "{}"
    scope = json.loads(raw) if isinstance(raw, str) else raw
    if scope.get("confirmed"):
        return scope.get("colleges", []), scope.get("entry_years", []), False
    labels = json.loads(doc["audience"]) if isinstance(doc["audience"], str) else doc["audience"]
    colleges, years, unknown = [], [], not bool(labels)
    for label in labels:
        if label in GENERAL:
            continue
        if label.endswith("学院"):
            colleges.append(label)
        elif match := re.fullmatch(r"(\d{4})(?:级|年入学)", label):
            years.append(match[1])
        else:
            unknown = True
    return colleges, years, unknown


def match_audience(doc, profile):
    colleges, years, unknown = constraints(doc)
    missing = []
    if unknown:
        missing.append("资料适用范围待管理员确认")
    for allowed, key, label in ((colleges, "college", "学院"), (years, "entry_year", "入学年份")):
        value = str(profile.get(key, "")).strip()
        if allowed and not value:
            missing.append(label)
        elif allowed and value not in allowed:
            return False, []
    return not missing, missing
