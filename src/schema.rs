//! `--output-schema` のための最小 JSON Schema バリデータ (#71)。
//!
//! 外部の `jsonschema` クレートは使わない。lodan が相手にするのは「フィールド名と型が決まった
//! 平たいオブジェクト」がほとんどで、小型モデルは `oneOf` や `$ref` を含むスキーマにそもそも
//! 従えない。1 機能のために引き込む依存としては重すぎる。
//!
//! 対応していないキーワードは**黙って無視しない**。無視すると「検証した気になって素通し」になるので、
//! スキーマを読む時点でエラーにする (壊れた hook の matcher や `extra_body` の予約キーと同じ方針)。
//!
//! 検証エラーはモデルへの再要求文にそのまま使うので、短く具体的に書く。

use serde_json::Value;

/// 値に意味を持たない注釈。読み飛ばす。
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
];

/// モデルに返す検証エラーの最大件数。全部並べても直せないので、先頭だけ。
const MAX_ERRORS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    String,
    Number,
    Integer,
    Boolean,
    Null,
    Object,
    Array,
}

impl Kind {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "string" => Kind::String,
            "number" => Kind::Number,
            "integer" => Kind::Integer,
            "boolean" => Kind::Boolean,
            "null" => Kind::Null,
            "object" => Kind::Object,
            "array" => Kind::Array,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Kind::String => "string",
            Kind::Number => "number",
            Kind::Integer => "integer",
            Kind::Boolean => "boolean",
            Kind::Null => "null",
            Kind::Object => "object",
            Kind::Array => "array",
        }
    }

    fn accepts(self, value: &Value) -> bool {
        match (self, value) {
            (Kind::String, Value::String(_))
            | (Kind::Boolean, Value::Bool(_))
            | (Kind::Null, Value::Null)
            | (Kind::Object, Value::Object(_))
            | (Kind::Array, Value::Array(_))
            | (Kind::Number, Value::Number(_)) => true,
            // 1.0 のように小数点つきで書かれた整数も整数として受ける (JSON Schema の定義どおり)。
            (Kind::Integer, Value::Number(n)) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        }
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Null => "null",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
    }
}

/// `additionalProperties`。
#[derive(Debug, Clone)]
enum Additional {
    Allowed,
    Forbidden,
    Matching(Box<Schema>),
}

/// 読み込み済みのスキーマ。
#[derive(Debug, Clone)]
pub struct Schema {
    kinds: Vec<Kind>,
    properties: Vec<(String, Schema)>,
    required: Vec<String>,
    additional: Additional,
    items: Option<Box<Schema>>,
    allowed: Option<Vec<Value>>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    exclusive_minimum: Option<f64>,
    exclusive_maximum: Option<f64>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    min_items: Option<u64>,
    max_items: Option<u64>,
}

impl Schema {
    /// スキーマを読む。対応していないキーワードや、形の違う値はエラー。
    pub fn parse(value: &Value) -> Result<Self, String> {
        Self::parse_at(value, "$")
    }

    fn parse_at(value: &Value, at: &str) -> Result<Self, String> {
        // `true` は「何でもよい」。`false` (何も通さない) は出力のスキーマとして意味が無い。
        if value == &Value::Bool(true) {
            return Ok(Self::anything());
        }
        let object = value.as_object().ok_or_else(|| {
            format!(
                "{at}: a schema must be a JSON object, got {}",
                kind_of(value)
            )
        })?;
        let mut schema = Self::anything();
        for (keyword, v) in object {
            let here = format!("{at}.{keyword}");
            match keyword.as_str() {
                k if ANNOTATIONS.contains(&k) => {}
                "type" => schema.kinds = parse_kinds(v, &here)?,
                "properties" => {
                    let props = v
                        .as_object()
                        .ok_or_else(|| format!("{here}: expected an object"))?;
                    for (name, sub) in props {
                        let sub = Self::parse_at(sub, &format!("{here}.{name}"))?;
                        schema.properties.push((name.clone(), sub));
                    }
                }
                "required" => schema.required = strings(v, &here)?,
                "additionalProperties" => {
                    schema.additional = match v {
                        Value::Bool(true) => Additional::Allowed,
                        Value::Bool(false) => Additional::Forbidden,
                        other => Additional::Matching(Box::new(Self::parse_at(other, &here)?)),
                    }
                }
                "items" => schema.items = Some(Box::new(Self::parse_at(v, &here)?)),
                "enum" => {
                    let values = v
                        .as_array()
                        .filter(|a| !a.is_empty())
                        .ok_or_else(|| format!("{here}: expected a non-empty array"))?;
                    schema.allowed = Some(values.clone());
                }
                "const" => schema.allowed = Some(vec![v.clone()]),
                "minimum" => schema.minimum = Some(number(v, &here)?),
                "maximum" => schema.maximum = Some(number(v, &here)?),
                "exclusiveMinimum" => schema.exclusive_minimum = Some(number(v, &here)?),
                "exclusiveMaximum" => schema.exclusive_maximum = Some(number(v, &here)?),
                "minLength" => schema.min_length = Some(count(v, &here)?),
                "maxLength" => schema.max_length = Some(count(v, &here)?),
                "minItems" => schema.min_items = Some(count(v, &here)?),
                "maxItems" => schema.max_items = Some(count(v, &here)?),
                other => {
                    return Err(format!(
                        "{here}: `{other}` is not supported by lodan's schema validator. Supported: \
                         type, properties, required, additionalProperties, items, enum, const, \
                         minimum, maximum, exclusiveMinimum, exclusiveMaximum, minLength, maxLength, \
                         minItems, maxItems"
                    ));
                }
            }
        }
        // 綴り違いで「必須のつもりが、誰も定義していない名前」になっていないか。
        if !schema.properties.is_empty() {
            for name in &schema.required {
                let defined = schema.properties.iter().any(|(n, _)| n == name);
                if !defined && matches!(schema.additional, Additional::Forbidden) {
                    return Err(format!(
                        "{at}.required: `{name}` is required but is not in `properties`, and \
                         additionalProperties is false — nothing could ever satisfy this"
                    ));
                }
            }
        }
        Ok(schema)
    }

    fn anything() -> Self {
        Self {
            kinds: Vec::new(),
            properties: Vec::new(),
            required: Vec::new(),
            additional: Additional::Allowed,
            items: None,
            allowed: None,
            minimum: None,
            maximum: None,
            exclusive_minimum: None,
            exclusive_maximum: None,
            min_length: None,
            max_length: None,
            min_items: None,
            max_items: None,
        }
    }

    /// `value` がスキーマに合わない点。空なら合格。多くても [`MAX_ERRORS`] 件。
    pub fn validate(&self, value: &Value) -> Vec<String> {
        let mut errors = Vec::new();
        self.check(value, "$", &mut errors);
        errors.truncate(MAX_ERRORS);
        errors
    }

    fn check(&self, value: &Value, at: &str, errors: &mut Vec<String>) {
        if errors.len() >= MAX_ERRORS {
            return;
        }
        if !self.kinds.is_empty() && !self.kinds.iter().any(|k| k.accepts(value)) {
            let wanted: Vec<&str> = self.kinds.iter().map(|k| k.name()).collect();
            errors.push(format!(
                "{at}: expected {}, got {}",
                wanted.join(" or "),
                kind_of(value)
            ));
            // 型が違えば、その先 (プロパティや長さ) を調べても雑音になるだけ。
            return;
        }
        if let Some(allowed) = &self.allowed
            && !allowed.contains(value)
        {
            let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
            errors.push(format!(
                "{at}: must be one of {}, got {value}",
                list.join(", ")
            ));
        }
        match value {
            Value::Object(map) => {
                for name in &self.required {
                    if !map.contains_key(name) {
                        errors.push(format!("{at}: missing required property `{name}`"));
                    }
                }
                for (name, v) in map {
                    let here = format!("{at}.{name}");
                    match self.properties.iter().find(|(n, _)| n == name) {
                        Some((_, sub)) => sub.check(v, &here, errors),
                        None => match &self.additional {
                            Additional::Allowed => {}
                            Additional::Forbidden => {
                                errors.push(format!("{at}: unexpected property `{name}`"));
                            }
                            Additional::Matching(sub) => sub.check(v, &here, errors),
                        },
                    }
                }
            }
            Value::Array(list) => {
                let n = list.len() as u64;
                if self.min_items.is_some_and(|min| n < min) {
                    errors.push(format!(
                        "{at}: needs at least {} item(s), got {n}",
                        self.min_items.unwrap_or_default()
                    ));
                }
                if self.max_items.is_some_and(|max| n > max) {
                    errors.push(format!(
                        "{at}: allows at most {} item(s), got {n}",
                        self.max_items.unwrap_or_default()
                    ));
                }
                if let Some(items) = &self.items {
                    for (i, v) in list.iter().enumerate() {
                        items.check(v, &format!("{at}[{i}]"), errors);
                    }
                }
            }
            Value::String(text) => {
                let n = text.chars().count() as u64;
                if self.min_length.is_some_and(|min| n < min) {
                    errors.push(format!(
                        "{at}: needs at least {} character(s), got {n}",
                        self.min_length.unwrap_or_default()
                    ));
                }
                if self.max_length.is_some_and(|max| n > max) {
                    errors.push(format!(
                        "{at}: allows at most {} character(s), got {n}",
                        self.max_length.unwrap_or_default()
                    ));
                }
            }
            Value::Number(number) => {
                let Some(n) = number.as_f64() else { return };
                let bounds = [
                    (self.minimum, n >= self.minimum.unwrap_or(n), ">="),
                    (self.maximum, n <= self.maximum.unwrap_or(n), "<="),
                    (
                        self.exclusive_minimum,
                        n > self.exclusive_minimum.unwrap_or(f64::NEG_INFINITY),
                        ">",
                    ),
                    (
                        self.exclusive_maximum,
                        n < self.exclusive_maximum.unwrap_or(f64::INFINITY),
                        "<",
                    ),
                ];
                for (bound, ok, op) in bounds {
                    if let Some(bound) = bound
                        && !ok
                    {
                        errors.push(format!("{at}: must be {op} {bound}, got {number}"));
                    }
                }
            }
            Value::Bool(_) | Value::Null => {}
        }
    }
}

fn parse_kinds(value: &Value, at: &str) -> Result<Vec<Kind>, String> {
    let names: Vec<&str> = match value {
        Value::String(s) => vec![s.as_str()],
        Value::Array(list) if !list.is_empty() => list
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| format!("{at}: expected type names"))
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(format!(
                "{at}: expected a type name or a non-empty list of them"
            ));
        }
    };
    names
        .into_iter()
        .map(|name| Kind::parse(name).ok_or_else(|| format!("{at}: unknown type `{name}`")))
        .collect()
}

fn strings(value: &Value, at: &str) -> Result<Vec<String>, String> {
    value
        .as_array()
        .and_then(|list| {
            list.iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| format!("{at}: expected a list of property names"))
}

fn number(value: &Value, at: &str) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("{at}: expected a number"))
}

fn count(value: &Value, at: &str) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("{at}: expected a non-negative integer"))
}

/// モデルの応答から JSON を取り出す。応答全体、```` ```json ```` のコードフェンスの中、最初の
/// `{` / `[` から対応する最後の閉じ括弧まで、の順に試す。小型モデルは「JSON だけを返せ」と
/// 言っても前置きやフェンスを付けがちで、そこで落とすのは惜しい。
pub fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Some(value);
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        // フェンスの 1 行目は言語名 (`json`)。
        let body = after.split_once('\n').map_or(after, |(_, rest)| rest);
        if let Some(end) = body.find("```")
            && let Ok(value) = serde_json::from_str(body[..end].trim())
        {
            return Some(value);
        }
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(start), Some(end)) = (trimmed.find(open), trimmed.rfind(close))
            && start < end
            && let Ok(value) = serde_json::from_str(&trimmed[start..=end])
        {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn review_schema() -> Schema {
        Schema::parse(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "review",
            "type": "object",
            "required": ["verdict", "findings"],
            "additionalProperties": false,
            "properties": {
                "verdict": { "enum": ["approve", "request_changes"] },
                "score": { "type": "integer", "minimum": 0, "maximum": 10 },
                "findings": {
                    "type": "array",
                    "maxItems": 3,
                    "items": {
                        "type": "object",
                        "required": ["file"],
                        "properties": {
                            "file": { "type": "string", "minLength": 1 },
                            "line": { "type": ["integer", "null"] }
                        }
                    }
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_matching_value_has_no_errors() {
        let ok = json!({
            "verdict": "approve",
            "score": 7.0,
            "findings": [{ "file": "src/main.rs", "line": null, "note": "extra is fine here" }]
        });
        assert_eq!(review_schema().validate(&ok), Vec::<String>::new());
    }

    #[test]
    fn errors_name_the_place_and_what_was_expected() {
        let bad = json!({
            "verdict": "lgtm",
            "score": 11,
            "findings": [{ "file": "" }, { "line": "12" }],
            "summary": "not in the schema"
        });
        let errors = review_schema().validate(&bad);
        for expected in [
            "$.verdict: must be one of \"approve\", \"request_changes\", got \"lgtm\"",
            "$.score: must be <= 10, got 11",
            "$.findings[0].file: needs at least 1 character(s), got 0",
            "$.findings[1]: missing required property `file`",
            "$.findings[1].line: expected integer or null, got string",
            "$: unexpected property `summary`",
        ] {
            assert!(
                errors.iter().any(|e| e == expected),
                "{expected}\n{errors:#?}"
            );
        }
        assert_eq!(errors.len(), 6, "{errors:#?}");
    }

    #[test]
    fn a_wrong_type_is_reported_once_without_descending() {
        let errors = review_schema().validate(&json!("just text"));
        assert_eq!(errors, ["$: expected object, got string"]);
        // 整数でない数は integer に合わない。
        let errors = review_schema()
            .validate(&json!({ "verdict": "approve", "score": 7.5, "findings": [] }));
        assert_eq!(errors, ["$.score: expected integer, got number"]);
    }

    #[test]
    fn the_error_list_is_capped() {
        let schema =
            Schema::parse(&json!({ "type": "array", "items": { "type": "string" } })).unwrap();
        let many: Vec<Value> = (0..50).map(|n| json!(n)).collect();
        assert_eq!(schema.validate(&Value::Array(many)).len(), MAX_ERRORS);
    }

    /// 対応していないキーワードを黙って無視すると、検証した気になって素通しになる。
    #[test]
    fn an_unsupported_keyword_is_rejected_when_the_schema_is_read() {
        for (schema, keyword) in [
            (json!({ "$ref": "#/definitions/x" }), "$ref"),
            (json!({ "oneOf": [{ "type": "string" }] }), "oneOf"),
            (json!({ "type": "string", "pattern": "^a" }), "pattern"),
            (json!({ "type": "string", "format": "email" }), "format"),
            (
                json!({ "type": "object", "properties": { "a": { "anyOf": [] } } }),
                "$.properties.a.anyOf",
            ),
        ] {
            let err = Schema::parse(&schema).unwrap_err();
            assert!(
                err.contains(keyword) && err.contains("not supported"),
                "{err}"
            );
        }
        for malformed in [
            json!({ "type": "strnig" }),
            json!({ "type": [] }),
            json!({ "required": "name" }),
            json!({ "enum": [] }),
            json!({ "minLength": -1 }),
            json!("object"),
            json!(false),
        ] {
            assert!(Schema::parse(&malformed).is_err(), "{malformed}");
        }
        // 満たしようのないスキーマ (必須なのに定義が無く、追加のプロパティも禁止)。
        let impossible = json!({
            "type": "object",
            "required": ["nmae"],
            "additionalProperties": false,
            "properties": { "name": { "type": "string" } }
        });
        assert!(Schema::parse(&impossible).unwrap_err().contains("nmae"));
    }

    #[test]
    fn json_is_found_inside_prose_and_code_fences() {
        let want = json!({ "ok": true });
        for reply in [
            r#"{"ok": true}"#,
            "  \n{\"ok\": true}\n",
            "Here you go:\n```json\n{\"ok\": true}\n```\nLet me know!",
            "```\n{\"ok\": true}\n```",
            "The answer is {\"ok\": true} as requested.",
        ] {
            assert_eq!(extract_json(reply), Some(want.clone()), "{reply}");
        }
        assert_eq!(extract_json("[1, 2]"), Some(json!([1, 2])));
        assert_eq!(extract_json("I could not do it."), None);
        assert_eq!(extract_json("{not json}"), None);
    }
}
