// MVP 外: 会話履歴の永続化・トークン会計。
// 現状の会話履歴は agent::Session (loop.rs) がメモリに保持する。
// 永続化やトークン集計を導入する際にここを実装する。

#[derive(Debug, Default, Clone)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub fn save_transcript(
    _path: &std::path::Path,
    _history: &[crate::agent::messages::Message],
) -> anyhow::Result<()> {
    unimplemented!("transcript persistence is out of MVP scope")
}
