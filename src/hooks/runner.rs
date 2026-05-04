// MVP 外: Hook ディスパッチ。

use super::{HookOutcome, Lifecycle};
use anyhow::Result;

pub async fn dispatch(_lc: Lifecycle, _payload: &serde_json::Value) -> Result<HookOutcome> {
    unimplemented!("hook dispatch is out of MVP scope")
}
