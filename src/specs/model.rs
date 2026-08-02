use serde::Deserialize;

use crate::{completion::SlotKind, terminal::RiskLevel};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecDocument {
    pub schema: u32,
    #[serde(default)]
    pub commands: Vec<CommandSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub description: String,
    pub platforms: Vec<String>,
    pub requires_arguments: bool,
    pub risk: SpecRisk,
    pub default: String,
    #[serde(default)]
    pub replaces: Option<String>,
    #[serde(default)]
    pub recipes: Vec<RecipeSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeSpec {
    pub id: String,
    pub template: String,
    pub description: String,
    pub risk: SpecRisk,
    pub activation: String,
    #[serde(default)]
    pub platforms: Vec<String>,
    #[serde(default)]
    pub slots: Vec<SpecSlot>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecSlot {
    pub name: String,
    pub kind: SpecSlotKind,
    pub provider: String,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub repeatable: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SpecRisk {
    ReadOnly,
    Low,
    Medium,
    High,
    Unknown,
}

impl From<SpecRisk> for RiskLevel {
    fn from(value: SpecRisk) -> Self {
        match value {
            SpecRisk::ReadOnly => Self::ReadOnly,
            SpecRisk::Low => Self::Low,
            SpecRisk::Medium => Self::Medium,
            SpecRisk::High => Self::High,
            SpecRisk::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SpecSlotKind {
    File,
    Directory,
    Path,
    Executable,
    NewFile,
    Process,
    Interface,
    Port,
    Value,
}

impl From<SpecSlotKind> for SlotKind {
    fn from(value: SpecSlotKind) -> Self {
        match value {
            SpecSlotKind::File => Self::File,
            SpecSlotKind::Directory => Self::Directory,
            SpecSlotKind::Path => Self::Path,
            SpecSlotKind::Executable => Self::Executable,
            SpecSlotKind::NewFile => Self::NewFile,
            SpecSlotKind::Process => Self::Process,
            SpecSlotKind::Interface => Self::Interface,
            SpecSlotKind::Port => Self::Port,
            SpecSlotKind::Value => Self::Value,
        }
    }
}
