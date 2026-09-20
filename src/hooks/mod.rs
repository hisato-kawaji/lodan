// Hooks: ライフサイクルイベント (UserPromptSubmit / PreToolUse / PostToolUse 等) で
// 外部コマンドを発火し、exit code でエージェントループを制御する。
// Claude Code の hook モデルに準拠した最小実装。

pub mod runner;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lifecycle {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    /// ツールがエラーを返したとき (PostToolUse に加えて発火する)。ブロックはできない。
    PostToolUseFailure,
    /// 承認プロンプトを出す直前。JSON の `decision` で allow / deny を返せる。
    PermissionRequest,
    /// lodan が利用者の注意を必要としているとき (承認待ちなど)。通知用で、出力は読まない。
    Notification,
    /// コンテキスト圧縮の前後。matcher は `manual` (`/compact`) か `auto`。PreCompact はブロックできる。
    PreCompact,
    PostCompact,
    /// `Task` の子エージェントの開始と終了。ブロックはできない。
    SubagentStart,
    SubagentStop,
    Stop,
}

/// 終了コードの解釈 (`hooks_compat`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HooksCompat {
    /// 旧挙動: 非 0 は全てブロック。stdout の JSON は読まない。
    V1,
    /// Claude Code 互換: **2 だけがブロック**、その他の非 0 は警告して続行。exit 0 の stdout が
    /// JSON オブジェクトなら判断として読む。
    #[default]
    V2,
}

/// `config.toml` の `[[hooks]]` 1 エントリ。
///
/// ```toml
/// [[hooks]]
/// event = "PreToolUse"
/// matcher = "Edit|Write"      # 省略可。空 / "*" は全て。下の `matches` を参照
/// command = "./scripts/guard.sh"
/// timeout_secs = 30           # 省略可
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// 任意の名前。後段のレイヤーが同じ `id` の hook を書けば置き換わり、`disabled_hooks` に
    /// 書けば無効になる (レイヤー間で hook は連結されるので、名前が無いと外す手段が無い)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub event: Lifecycle,
    #[serde(default)]
    pub matcher: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// matcher がこの文字だけなら正規表現ではなく「完全一致の並び」として読む (Claude Code と同じ判定)。
fn is_exact_list(matcher: &str) -> bool {
    matcher
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ' ' | ',' | '|'))
}

impl HookConfig {
    /// `subject` (PreToolUse / PostToolUse ならツール名) がこの hook の matcher に一致するか。
    ///
    /// - 空 / `"*"`: 常に一致
    /// - 英数と `_ - 空白 , |` だけ: 完全一致、または `|` `,` 区切りの並びのどれかに完全一致
    /// - それ以外: 正規表現。**部分一致** (`Edit.*` は `NotebookEdit` にも当たる。全体一致は `^Edit$`)
    ///
    /// `subject` が None のイベント (UserPromptSubmit 等) では matcher を無視して一致扱い。
    /// 壊れた正規表現は起動時に [`HookConfig::validate`] で弾くので、ここでは一致しない扱いで足りる。
    pub fn matches(&self, subject: Option<&str>) -> bool {
        let Some(subject) = subject else {
            return true;
        };
        let matcher = self.matcher.trim();
        if matcher.is_empty() || matcher == "*" {
            return true;
        }
        if is_exact_list(matcher) {
            return matcher.split(['|', ',']).any(|m| m.trim() == subject);
        }
        use grep_matcher::Matcher;
        grep_regex::RegexMatcher::new(matcher)
            .ok()
            .and_then(|re| re.is_match(subject.as_bytes()).ok())
            .unwrap_or(false)
    }

    /// 起動時の検査。黙って一度も発火しない guard hook が一番まずいので、壊れた matcher は起動を止める。
    pub fn validate(&self) -> Result<(), String> {
        let matcher = self.matcher.trim();
        if matcher.is_empty() || matcher == "*" || is_exact_list(matcher) {
            return Ok(());
        }
        grep_regex::RegexMatcher::new(matcher)
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "hook matcher `{matcher}` (command `{}`) is not a valid regular expression: {e}",
                    self.command
                )
            })
    }
}

/// 実際に発火させる hook の並び。`disabled` に挙がった `id` を外し、同じ `id` が複数あれば
/// **最後のもの**だけを残す (位置は最後のものの位置)。`id` の無い hook はそのまま。
pub fn effective(hooks: &[HookConfig], disabled: &[String]) -> Vec<HookConfig> {
    hooks
        .iter()
        .enumerate()
        .filter(|(i, hook)| match &hook.id {
            None => true,
            Some(id) => {
                !disabled.contains(id)
                    && !hooks[i + 1..]
                        .iter()
                        .any(|later| later.id.as_ref() == Some(id))
            }
        })
        .map(|(_, hook)| hook.clone())
        .collect()
}

#[async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, lc: Lifecycle, payload: &serde_json::Value) -> anyhow::Result<HookOutcome>;
}

/// PreToolUse の hook が承認ゲートへ伝える希望。`deny` はブロックとして扱うのでここには無い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionHint {
    /// 承認プロンプトを省いてよい。**deny / ask ルールには勝てない**。
    Allow,
    /// 他の条件で通る呼び出しでも、必ずユーザーに尋ねる。
    Ask,
}

/// 一致した hook 全部の結果を畳んだもの。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HookOutcome {
    /// いずれかの hook が止めた。理由つき。以降の hook は実行していない。
    pub block: Option<String>,
    /// 複数の hook が違う希望を出したら ask が allow に勝つ。
    pub permission: Option<PermissionHint>,
    /// 書き換えられたツール入力 (PreToolUse)。複数あれば最後の hook のもの。
    pub updated_input: Option<serde_json::Value>,
    /// モデルに見せる追加の文脈 (`additionalContext`)。
    pub context: Vec<String>,
}

impl HookOutcome {
    pub fn blocked(reason: impl Into<String>) -> Self {
        Self {
            block: Some(reason.into()),
            ..Self::default()
        }
    }

    /// `context` を 1 つの文字列に。無ければ None。
    pub fn joined_context(&self) -> Option<String> {
        (!self.context.is_empty()).then(|| self.context.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(id: Option<&str>, command: &str) -> HookConfig {
        HookConfig {
            id: id.map(str::to_string),
            event: Lifecycle::PreToolUse,
            matcher: String::new(),
            command: command.to_string(),
            timeout_secs: None,
        }
    }

    #[test]
    fn a_later_hook_with_the_same_id_replaces_the_earlier_one_and_disabled_ids_are_dropped() {
        // レイヤーの順 (ユーザ設定 → プロジェクト設定) に連結された並び。
        let hooks = [
            named(Some("lint"), "user-lint"),
            named(None, "user-anonymous"),
            named(Some("notify"), "user-notify"),
            named(Some("lint"), "project-lint"),
        ];
        let commands = |disabled: &[&str]| -> Vec<String> {
            let disabled: Vec<String> = disabled.iter().map(|s| s.to_string()).collect();
            effective(&hooks, &disabled)
                .into_iter()
                .map(|h| h.command)
                .collect()
        };
        assert_eq!(
            commands(&[]),
            ["user-anonymous", "user-notify", "project-lint"]
        );
        assert_eq!(commands(&["notify"]), ["user-anonymous", "project-lint"]);
        assert_eq!(
            commands(&["lint", "nope"]),
            ["user-anonymous", "user-notify"]
        );
    }
}
