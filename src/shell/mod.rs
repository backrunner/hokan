mod init;
mod kind;
mod protocol;
mod session;

pub use init::{PROTOCOL_VERSION, init_script};
pub use kind::ShellKind;
pub use protocol::{ProtocolDiagnostic, ShellEvent, ShellProtocolDecoder};
pub use session::{ControlMessage, ControlReader, ShellSession};

pub const ZSH_REPLACEMENT_SEQUENCE: &[u8] = b"\x1b[99~";
pub const BASH_REPLACEMENT_SEQUENCE: &[u8] = b"\x18\x1d";
pub const FISH_REPLACEMENT_SEQUENCE: &[u8] = b"\x1b[99~";

#[must_use]
pub const fn replacement_sequence(shell: ShellKind) -> &'static [u8] {
    match shell {
        ShellKind::Zsh => ZSH_REPLACEMENT_SEQUENCE,
        ShellKind::Bash => BASH_REPLACEMENT_SEQUENCE,
        ShellKind::Fish => FISH_REPLACEMENT_SEQUENCE,
    }
}
