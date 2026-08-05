mod aliases;
mod function_slot;
mod init;
mod kind;
mod protocol;
mod session;

#[cfg(test)]
pub(crate) use aliases::parse_rc_text;
pub use aliases::{AliasCache, AliasEntry, AliasKind, ShellAliases};
pub(crate) use function_slot::{FunctionSlot, infer_function_slot};
pub use init::{PROTOCOL_VERSION, init_script};
pub use kind::ShellKind;
pub use protocol::{ProtocolDiagnostic, ShellEvent, ShellProtocolDecoder};
pub use session::{ControlMessage, ControlReader, ShellSession};

pub const ZSH_REPLACEMENT_SEQUENCE: &[u8] = b"\x1b[99~";
pub const BASH_REPLACEMENT_SEQUENCE: &[u8] = b"\x18\x1d";
pub const FISH_REPLACEMENT_SEQUENCE: &[u8] = b"\x1b[99~";
/// Zsh-only sibling of the replacement sequence: the bound widget applies the
/// pending edit and then accepts the line (executes it).
pub const ZSH_ACCEPT_SEQUENCE: &[u8] = b"\x1b[98~";

#[must_use]
pub const fn replacement_sequence(shell: ShellKind) -> &'static [u8] {
    match shell {
        ShellKind::Zsh => ZSH_REPLACEMENT_SEQUENCE,
        ShellKind::Bash => BASH_REPLACEMENT_SEQUENCE,
        ShellKind::Fish => FISH_REPLACEMENT_SEQUENCE,
    }
}

/// Sequence that applies the pending edit and immediately accepts (executes)
/// the resulting command line. Only zsh has a dedicated widget; bash uses
/// keystroke replay plus a literal Enter and fish appends `\r` after the
/// replacement sequence instead.
#[must_use]
pub const fn accept_sequence(shell: ShellKind) -> Option<&'static [u8]> {
    match shell {
        ShellKind::Zsh => Some(ZSH_ACCEPT_SEQUENCE),
        ShellKind::Bash | ShellKind::Fish => None,
    }
}
