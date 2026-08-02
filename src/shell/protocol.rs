use std::path::PathBuf;

use super::ShellKind;
use crate::terminal::BoundaryId;

const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellEvent {
    Prompt {
        boundary_id: BoundaryId,
        cwd: PathBuf,
        history_control: Option<String>,
    },
    Buffer {
        redisplay_id: BoundaryId,
        cursor: usize,
        text: String,
    },
    CommandStart {
        command: String,
    },
    CommandEnd {
        exit_code: i32,
        cwd: PathBuf,
        command: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolDiagnostic {
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DecodedShellEvents {
    pub events: Vec<ShellEvent>,
    pub diagnostics: Vec<ProtocolDiagnostic>,
    pub buffer_uncertain: bool,
}

#[derive(Debug)]
pub struct ShellProtocolDecoder {
    shell: ShellKind,
    pending: Vec<u8>,
    dropping_oversized: bool,
    last_prompt_id: u64,
    last_redisplay_id: u64,
    active_command: Option<String>,
}

impl ShellProtocolDecoder {
    #[must_use]
    pub const fn new(shell: ShellKind) -> Self {
        Self {
            shell,
            pending: Vec::new(),
            dropping_oversized: false,
            last_prompt_id: 0,
            last_redisplay_id: 0,
            active_command: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> DecodedShellEvents {
        let mut output = DecodedShellEvents::default();
        for &byte in bytes {
            if self.dropping_oversized {
                if byte == 0 {
                    self.dropping_oversized = false;
                }
                continue;
            }
            if byte == 0 {
                if !self.pending.is_empty() {
                    let frame = std::mem::take(&mut self.pending);
                    self.decode_frame(&frame, &mut output);
                }
                continue;
            }
            self.pending.push(byte);
            if self.pending.len() > MAX_FRAME_BYTES {
                self.pending.clear();
                self.dropping_oversized = true;
                output.buffer_uncertain = true;
                output.diagnostics.push(ProtocolDiagnostic {
                    code: "HK-SHL-004",
                    message: "shell control frame exceeded 64 KiB".into(),
                });
            }
        }
        output
    }

    fn decode_frame(&mut self, frame: &[u8], output: &mut DecodedShellEvents) {
        let Ok(frame) = std::str::from_utf8(frame) else {
            output.buffer_uncertain = true;
            output.diagnostics.push(ProtocolDiagnostic {
                code: "HK-SHL-005",
                message: "shell control frame was not valid UTF-8".into(),
            });
            return;
        };
        let mut envelope = frame.splitn(3, '\t');
        if envelope.next() != Some("HKP2") {
            output.diagnostics.push(ProtocolDiagnostic {
                code: "HK-SHL-003",
                message: "ignored shell control frame with an unknown protocol version".into(),
            });
            return;
        }
        let event = envelope.next().unwrap_or_default();
        let payload = envelope.next().unwrap_or_default();
        let decoded = match event {
            "PROMPT" => self.decode_prompt(payload),
            "BUFFER" => self.decode_buffer(payload),
            "START" => self.decode_start(payload),
            "END" => self.decode_end(payload),
            _ => Err((
                "HK-SHL-006",
                format!("ignored unknown shell event {event:?}"),
            )),
        };
        match decoded {
            Ok(event) => output.events.push(event),
            Err((code, message)) => {
                if event == "BUFFER" {
                    output.buffer_uncertain = true;
                }
                output
                    .diagnostics
                    .push(ProtocolDiagnostic { code, message });
            }
        }
    }

    fn decode_prompt(&mut self, payload: &str) -> Result<ShellEvent, (&'static str, String)> {
        let (id, payload) = payload.split_once('\t').ok_or_else(|| {
            (
                "HK-SHL-010",
                "prompt event is missing its working directory".into(),
            )
        })?;
        let id = parse_monotonic_id(Some(id), self.last_prompt_id, "prompt")?;
        let (cwd, history_control) = match self.shell {
            ShellKind::Bash => {
                let (history_control, cwd) = payload.split_once('\t').ok_or_else(|| {
                    (
                        "HK-SHL-010",
                        "bash prompt event is missing its working directory".into(),
                    )
                })?;
                (
                    cwd,
                    (!history_control.is_empty()).then(|| history_control.to_owned()),
                )
            }
            ShellKind::Zsh | ShellKind::Fish => (payload, None),
        };
        self.last_prompt_id = id;
        self.active_command = None;
        Ok(ShellEvent::Prompt {
            boundary_id: BoundaryId::new(id),
            cwd: PathBuf::from(cwd),
            history_control,
        })
    }

    fn decode_buffer(&mut self, payload: &str) -> Result<ShellEvent, (&'static str, String)> {
        let mut fields = payload.splitn(3, '\t');
        let id = parse_monotonic_id(fields.next(), self.last_redisplay_id, "redisplay")?;
        let cursor = fields
            .next()
            .ok_or_else(|| ("HK-SHL-007", "buffer event is missing its cursor".into()))?
            .parse::<usize>()
            .map_err(|_| ("HK-SHL-007", "buffer cursor is not an integer".into()))?;
        let text = fields.next().unwrap_or_default().to_owned();
        let cursor = match self.shell {
            ShellKind::Zsh => char_offset_to_byte(&text, cursor),
            ShellKind::Bash | ShellKind::Fish => Some(cursor),
        }
        .filter(|offset| *offset <= text.len() && text.is_char_boundary(*offset))
        .ok_or_else(|| {
            (
                "HK-SHL-008",
                "buffer cursor does not fall on a UTF-8 boundary".into(),
            )
        })?;
        self.last_redisplay_id = id;
        Ok(ShellEvent::Buffer {
            redisplay_id: BoundaryId::new(id),
            cursor,
            text,
        })
    }

    fn decode_start(&mut self, command: &str) -> Result<ShellEvent, (&'static str, String)> {
        if self.active_command.is_some() {
            return Err((
                "HK-SHL-013",
                "ignored command start event while another command is active".into(),
            ));
        }
        self.active_command = Some(command.to_owned());
        Ok(ShellEvent::CommandStart {
            command: command.to_owned(),
        })
    }

    fn decode_end(&mut self, payload: &str) -> Result<ShellEvent, (&'static str, String)> {
        let (exit_code, cwd) = payload.split_once('\t').ok_or_else(|| {
            (
                "HK-SHL-009",
                "command end event is missing its working directory".into(),
            )
        })?;
        let exit_code = exit_code
            .parse::<i32>()
            .map_err(|_| ("HK-SHL-009", "command exit code is not an integer".into()))?;
        let command = self.active_command.take().ok_or_else(|| {
            (
                "HK-SHL-013",
                "ignored command end event without a matching start".into(),
            )
        })?;
        Ok(ShellEvent::CommandEnd {
            exit_code,
            cwd: PathBuf::from(cwd),
            command,
        })
    }
}

fn parse_monotonic_id(
    value: Option<&str>,
    previous: u64,
    kind: &str,
) -> Result<u64, (&'static str, String)> {
    let id = value
        .ok_or_else(|| ("HK-SHL-010", format!("{kind} event is missing its id")))?
        .parse::<u64>()
        .map_err(|_| ("HK-SHL-010", format!("{kind} id is not an integer")))?;
    if id <= previous {
        return Err((
            "HK-SHL-011",
            format!("{kind} id {id} is not newer than {previous}"),
        ));
    }
    Ok(id)
}

fn char_offset_to_byte(text: &str, offset: usize) -> Option<usize> {
    if offset == text.chars().count() {
        return Some(text.len());
    }
    text.char_indices().nth(offset).map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_arbitrary_chunks_and_zsh_character_cursor() {
        let frame = "HKP2\tBUFFER\t1\t2\ta中b\0";
        for split in 0..=frame.len() {
            let mut decoder = ShellProtocolDecoder::new(ShellKind::Zsh);
            let mut events = decoder.feed(&frame.as_bytes()[..split]).events;
            events.extend(decoder.feed(&frame.as_bytes()[split..]).events);
            assert_eq!(
                events,
                vec![ShellEvent::Buffer {
                    redisplay_id: BoundaryId::new(1),
                    cursor: 4,
                    text: "a中b".into(),
                }]
            );
        }
    }

    #[test]
    fn rejects_non_monotonic_and_oversized_frames() {
        let mut decoder = ShellProtocolDecoder::new(ShellKind::Bash);
        assert_eq!(
            decoder
                .feed(b"HKP2\tPROMPT\t1\tignoreboth\t/tmp\0")
                .events
                .len(),
            1
        );
        let duplicate = decoder.feed(b"HKP2\tPROMPT\t1\tignoreboth\t/tmp\0");
        assert!(duplicate.events.is_empty());
        assert_eq!(duplicate.diagnostics[0].code, "HK-SHL-011");

        let oversized = decoder.feed(&vec![b'x'; MAX_FRAME_BYTES + 1]);
        assert!(oversized.buffer_uncertain);
    }

    #[test]
    fn decodes_optional_bash_history_control() {
        let mut decoder = ShellProtocolDecoder::new(ShellKind::Bash);
        assert_eq!(
            decoder
                .feed(b"HKP2\tPROMPT\t1\tignoreboth:erasedups\t/tmp\0")
                .events,
            vec![ShellEvent::Prompt {
                boundary_id: BoundaryId::new(1),
                cwd: PathBuf::from("/tmp"),
                history_control: Some("ignoreboth:erasedups".into()),
            }]
        );
    }

    #[test]
    fn preserves_tabs_in_last_payload_fields() {
        let mut decoder = ShellProtocolDecoder::new(ShellKind::Bash);
        let output = decoder.feed(
            b"HKP2\tBUFFER\t1\t4\techo\tvalue\0HKP2\tSTART\techo\tvalue\0\
              HKP2\tEND\t0\t/tmp\twith-tab\0",
        );
        assert_eq!(
            output.events,
            vec![
                ShellEvent::Buffer {
                    redisplay_id: BoundaryId::new(1),
                    cursor: 4,
                    text: "echo\tvalue".into(),
                },
                ShellEvent::CommandStart {
                    command: "echo\tvalue".into(),
                },
                ShellEvent::CommandEnd {
                    exit_code: 0,
                    cwd: PathBuf::from("/tmp\twith-tab"),
                    command: "echo\tvalue".into(),
                },
            ]
        );
    }

    #[test]
    fn prompt_working_directories_preserve_tabs() {
        let mut zsh = ShellProtocolDecoder::new(ShellKind::Zsh);
        assert_eq!(
            zsh.feed(b"HKP2\tPROMPT\t1\t/tmp\twith-tab\0").events,
            vec![ShellEvent::Prompt {
                boundary_id: BoundaryId::new(1),
                cwd: PathBuf::from("/tmp\twith-tab"),
                history_control: None,
            }]
        );

        let mut bash = ShellProtocolDecoder::new(ShellKind::Bash);
        assert_eq!(
            bash.feed(b"HKP2\tPROMPT\t1\tignoreboth\t/tmp\twith-tab\0")
                .events,
            vec![ShellEvent::Prompt {
                boundary_id: BoundaryId::new(1),
                cwd: PathBuf::from("/tmp\twith-tab"),
                history_control: Some("ignoreboth".into()),
            }]
        );
    }

    #[test]
    fn rejects_unmatched_command_end_without_corrupting_the_next_command() {
        let mut decoder = ShellProtocolDecoder::new(ShellKind::Zsh);
        let unmatched = decoder.feed(b"HKP2\tEND\t0\t/tmp\0");
        assert!(unmatched.events.is_empty());
        assert_eq!(unmatched.diagnostics[0].code, "HK-SHL-013");

        let recovered = decoder.feed(b"HKP2\tSTART\techo ok\0HKP2\tEND\t0\t/tmp\twith-tab\0");
        assert_eq!(
            recovered.events,
            vec![
                ShellEvent::CommandStart {
                    command: "echo ok".into(),
                },
                ShellEvent::CommandEnd {
                    exit_code: 0,
                    cwd: PathBuf::from("/tmp\twith-tab"),
                    command: "echo ok".into(),
                },
            ]
        );
    }
}
