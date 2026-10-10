//! elicitation: MCP サーバが利用者に入力を求める server→client リクエスト (`elicitation/create`、#83)。
//!
//! サーバは `message` と `requestedSchema` (トップレベルが object の JSON Schema。各プロパティは
//! string / number / integer / boolean / enum の素朴な型) を送り、クライアントは
//! `{ action: accept | decline | cancel, content? }` を返す。lodan は REPL なら AskUserQuestion と
//! 同じ経路 (stdin) で 1 項目ずつ尋ね、`-p` (尋ねる相手がいない) では decline を返す。
//! capability は REPL のときだけ広告する。

use std::io::{self, BufRead, Write};

use serde_json::{Map, Value};

pub struct ElicitationProvider {
    server: String,
}

impl ElicitationProvider {
    pub fn new(server: &str) -> Self {
        Self {
            server: server.to_string(),
        }
    }

    /// `elicitation/create` の params を受けて応答を作る。失敗は decline に倒す (サーバの要求で
    /// ツール呼び出しを止めない)。
    pub async fn create(&self, params: Value) -> Value {
        // ヘッドレスでは stdin がプロンプトに使われ、stdout は機械可読。尋ねない。
        if crate::term::display_to_stderr() {
            return decline();
        }
        let server = self.server.clone();
        tokio::task::spawn_blocking(move || prompt(&server, &params))
            .await
            .unwrap_or_else(|_| decline())
    }
}

impl ElicitationProvider {
    /// 尋ねる相手がいないときの応答 (capability を広告していないサーバから来たときにも使う)。
    pub fn declined() -> Value {
        decline()
    }
}

fn decline() -> Value {
    serde_json::json!({ "action": "decline" })
}

/// stdin で 1 項目ずつ尋ねる。空行は「答えない」: 必須なら全体を decline、任意なら飛ばす。
/// EOF も decline。型に合わない答えは言い直させる。
fn prompt(server: &str, params: &Value) -> Value {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("(no message)");
    let schema = params
        .get("requestedSchema")
        .cloned()
        .unwrap_or(Value::Null);
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    // サーバの文字列は信用しない (端末のエスケープを無害化する)。
    let _ = writeln!(
        stdout,
        "[mcp:{}] asks: {}\n(empty answer = don't answer; required fields left empty decline the whole request)",
        crate::term::sanitize(server),
        crate::term::sanitize(message)
    );
    // 尋ねる項目が無い (schema が無い / object でない / properties が空) ときは「承諾するか」だけを
    // 確かめる。黙って accept を返さない (#143 のレビュー)。
    if properties.is_empty() {
        let _ = write!(stdout, "  accept? [y/N]> ");
        let _ = stdout.flush();
        let mut line = String::new();
        return match stdin.lock().read_line(&mut line) {
            Ok(n)
                if n > 0
                    && parse_value(&serde_json::json!({ "type": "boolean" }), line.trim())
                        == Ok(Value::Bool(true)) =>
            {
                serde_json::json!({ "action": "accept", "content": {} })
            }
            _ => decline(),
        };
    }
    // serde_json の Map はキー順なのでサーバが書いた順は分からない。必須のものを `required` の順に
    // 先に、残りをキー順に尋ねる。
    let ordered: Vec<(&String, &Value)> = required
        .iter()
        .filter_map(|r| properties.get_key_value(*r))
        .chain(
            properties
                .iter()
                .filter(|(n, _)| !required.contains(&n.as_str())),
        )
        .collect();
    let mut content = Map::new();
    for (name, field) in ordered {
        let is_required = required.contains(&name.as_str());
        loop {
            let _ = write!(
                stdout,
                "  {}{} {}> ",
                crate::term::sanitize(name),
                if is_required { " (required)" } else { "" },
                crate::term::sanitize(&describe(field))
            );
            let _ = stdout.flush();
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => return decline(),
                Ok(_) => {}
            }
            let raw = line.trim();
            if raw.is_empty() {
                if is_required {
                    return decline();
                }
                break;
            }
            match parse_value(field, raw) {
                Ok(v) => {
                    content.insert(name.clone(), v);
                    break;
                }
                Err(why) => {
                    let _ = writeln!(stdout, "  {why}");
                }
            }
        }
    }
    serde_json::json!({ "action": "accept", "content": Value::Object(content) })
}

/// プロンプトに添える型の説明 (`(string)`、`(one of: a | b)`、`(integer) 説明`)。
fn describe(field: &Value) -> String {
    let kind = if let Some(values) = field.get("enum").and_then(Value::as_array) {
        format!(
            "one of: {}",
            values
                .iter()
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                .collect::<Vec<_>>()
                .join(" | ")
        )
    } else {
        field
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("string")
            .to_string()
    };
    match field.get("description").and_then(Value::as_str) {
        Some(d) if !d.is_empty() => format!("({kind}) {d}"),
        _ => format!("({kind})"),
    }
}

/// 答えの文字列を requestedSchema のプロパティの型に合わせる。enum は候補との完全一致 (大小無視)、
/// boolean は yes/no/true/false/y/n、integer / number は数値。それ以外は文字列のまま。
pub fn parse_value(field: &Value, raw: &str) -> Result<Value, String> {
    if let Some(values) = field.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .find(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(raw)))
            .cloned()
            .ok_or_else(|| "please answer with one of the listed values".to_string());
    }
    match field
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("string")
    {
        "boolean" => match raw.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" => Ok(Value::Bool(true)),
            "n" | "no" | "false" => Ok(Value::Bool(false)),
            _ => Err("please answer yes or no".to_string()),
        },
        "integer" => raw
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "please answer with an integer".to_string()),
        "number" => raw
            .parse::<f64>()
            .ok()
            .and_then(|f| serde_json::Number::from_f64(f).map(Value::Number))
            .ok_or_else(|| "please answer with a number".to_string()),
        _ => Ok(Value::String(raw.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_coerced_to_the_requested_type() {
        let s = |t: &str| serde_json::json!({ "type": t });
        assert_eq!(parse_value(&s("string"), "Ada").unwrap(), "Ada");
        assert_eq!(parse_value(&s("integer"), "42").unwrap(), 42);
        assert!(parse_value(&s("integer"), "4.2").is_err());
        assert_eq!(parse_value(&s("number"), "4.5").unwrap(), 4.5);
        assert_eq!(parse_value(&s("boolean"), "Yes").unwrap(), true);
        assert_eq!(parse_value(&s("boolean"), "n").unwrap(), false);
        assert!(parse_value(&s("boolean"), "maybe").is_err());
        let e = serde_json::json!({ "type": "string", "enum": ["dev", "prod"] });
        assert_eq!(parse_value(&e, "PROD").unwrap(), "prod");
        assert!(parse_value(&e, "staging").is_err());
        assert_eq!(describe(&e), "(one of: dev | prod)");
        assert_eq!(
            describe(&serde_json::json!({ "type": "integer", "description": "your age" })),
            "(integer) your age"
        );
    }
}
