use super::SlotKind;
use std::path::PathBuf;

/// Meaning of the active argument, resolved once before providers run.
/// Unknown is not permission to probe paths, even when a matching directory exists.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ArgumentSemantics {
    #[default]
    Unknown,
    Text,
    /// A path position whose shell expansion cannot safely be resolved.
    UnresolvedPath,
    LocalPath(ResolvedPath),
}

impl ArgumentSemantics {
    #[must_use]
    pub const fn local_path(&self) -> Option<&ResolvedPath> {
        match self {
            Self::LocalPath(path) => Some(path),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathOrigin {
    Redirection,
    Executable,
    CommandContract,
}

/// A concrete filesystem request. Providers consume this contract instead of
/// guessing from slashes, help prose, or the existence of local directories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPath {
    pub kind: SlotKind,
    pub origin: PathOrigin,
    pub directory: PathBuf,
    pub prefix: String,
    pub edit_prefix: String,
    pub base_dir: PathBuf,
    pub literal_root: bool,
    pub(crate) file_next_slot: Option<SlotKind>,
    pub(crate) cd_target: bool,
    /// Empty generic operands can defer to command recipes; required option
    /// values keep file rows even when the command also has recipes.
    pub(crate) defer_to_recipes: bool,
}
