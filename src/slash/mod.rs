// MVP 外: slash command 拡張 (REPL 組み込み builtins は repl.rs にある)。

use anyhow::Result;

pub struct SlashCommand {
    pub name: String,
    pub body: String,
}

pub fn register_user_commands(_cmds: &[SlashCommand]) -> Result<()> {
    unimplemented!("slash command extensions are out of MVP scope")
}
