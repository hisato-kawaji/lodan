// MVP 外: skills ロード。

use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub instructions: String,
}

pub fn load_from(_dir: &Path) -> Result<Vec<Skill>> {
    unimplemented!("skills loading is out of MVP scope")
}
