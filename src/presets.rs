//! モデル別プリセット (#65 E)。**測って分かった設定だけ**を名前で呼べるようにする。
//!
//! 小型ローカルモデルは「どのフラグを立てるか」で所要時間と正答が大きく変わるが、効く組み合わせは
//! モデルごとに違い、推測は当たらない (`tool_choice` は単一方針が成立しないことを実測した)。
//! ここに載せるのは `docs/eval/` に実測が残っているものに限る。
//!
//! **モデル名から自動で当てない**。利用者が書いた設定を黙って変えないため、`preset = "<name>"` の
//! 明示指定だけで効き、同じキーを利用者が書いていればそちらが勝つ (レイヤーは
//! 「プリセット → 設定ファイル → CLI」の順に強い)。

use serde::{Deserialize, Serialize};

/// プリセット 1 つ。`None` のキーには触らない。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: &'static str,
    /// 何に効くかと、根拠の実測。
    pub about: &'static str,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<&'static str>,
    pub tool_choice: Option<&'static str>,
    pub tool_profile: Option<crate::config::ToolProfile>,
}

/// 組み込みプリセット。追加するときは `docs/eval/` の実測を `about` に書くこと。
pub fn all() -> Vec<Preset> {
    vec![
        Preset {
            name: "local-small",
            about: "ローカルの 9B 前後 (qwen3.5:9b で実測): thinking off で正答は同じまま 28% 速い。\
                    thinking ありは 20 実行中 13 が「思考だけで本文の無い応答」で終わった \
                    (docs/eval/ladder/qwen3.5-thinking-core-2026-09.md)",
            temperature: Some(0.2),
            reasoning_effort: Some("none"),
            tool_choice: None,
            tool_profile: None,
        },
        Preset {
            name: "local-small-core",
            about: "local-small に加えてツール定義を 7 個に絞る。MCP を多数つないだ構成では定義が \
                    20.6 KB → 12.9 KB、推定トークン -59% (#72 の実測)",
            temperature: Some(0.2),
            reasoning_effort: Some("none"),
            tool_choice: None,
            tool_profile: Some(crate::config::ToolProfile::Core),
        },
        Preset {
            name: "kimi-k3",
            about: "Moonshot kimi-k3: effort=low で正答 10/10 のまま思考が -73% \
                    (docs/eval/ladder/kimi-k3-effort-2026-09.md)。サーバ既定は max",
            temperature: None,
            reasoning_effort: Some("low"),
            tool_choice: None,
            tool_profile: None,
        },
        Preset {
            name: "forced-tools",
            about: "ツールコールをテキストで出してしまうモデル (Qwen2.5-Coder 7B で実測) 向け: \
                    tool_choice=required で必ずツール呼び出しにさせる。**GLM-4.7-Flash のように \
                    required だと応答が返らないモデルもあるので、当てる前に 1 回試すこと**",
            temperature: Some(0.2),
            reasoning_effort: None,
            tool_choice: Some("required"),
            tool_profile: None,
        },
    ]
}

pub fn find(name: &str) -> Option<Preset> {
    all().into_iter().find(|p| p.name == name)
}

/// 一覧 (`lodan presets`)。
pub fn describe_all() -> String {
    let mut out = String::from(
        "built-in presets (apply with `[llm.<provider>] preset = \"<name>\"`; keys you set \
         yourself win):\n",
    );
    for p in all() {
        out.push_str(&format!("\n{}\n  {}\n  sets:", p.name, p.about));
        let mut sets = Vec::new();
        if let Some(t) = p.temperature {
            sets.push(format!("temperature = {t}"));
        }
        if let Some(e) = p.reasoning_effort {
            sets.push(format!("reasoning_effort = \"{e}\""));
        }
        if let Some(c) = p.tool_choice {
            sets.push(format!("tool_choice = \"{c}\""));
        }
        if let Some(profile) = p.tool_profile {
            sets.push(format!("tool_profile = \"{}\"", profile.as_str()));
        }
        out.push_str(&format!(" {}\n", sets.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_has_a_measurement_in_its_about_and_sets_something() {
        for p in all() {
            assert!(
                p.about.contains("実測") || p.about.contains("docs/eval") || p.about.contains('#'),
                "{}: about に根拠が無い",
                p.name
            );
            assert!(
                p.temperature.is_some()
                    || p.reasoning_effort.is_some()
                    || p.tool_choice.is_some()
                    || p.tool_profile.is_some(),
                "{}: 何も設定していない",
                p.name
            );
        }
        assert!(find("local-small").is_some());
        assert!(find("nope").is_none());
        let listed = describe_all();
        assert!(listed.contains("local-small") && listed.contains("reasoning_effort = \"none\""));
    }
}
