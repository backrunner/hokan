/// Merge ordinary spacing variants without changing shell arguments. Quoted,
/// escaped, multiline, or expansion-bearing commands retain their spelling:
/// whitespace there may be data, a command boundary, or nested shell syntax.
pub(super) fn normalize_command(command: &str) -> String {
    if command.contains(['\'', '"', '\\', '\n', '\r', '`', '$', '#']) {
        return command.trim_start_matches([' ', '\t']).to_owned();
    }
    command
        .split([' ', '\t'])
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
