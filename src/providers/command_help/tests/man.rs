use super::*;

#[test]
fn parses_tar_style_flags_with_inline_and_block_descriptions() {
    let page = [
        bold("NAME"),
        String::new(),
        bold("DESCRIPTION"),
        format!(
            "     {}      Create a new archive containing the specified items.",
            bold("-c")
        ),
        String::new(),
        "     In other modes, files are added in order.".to_owned(),
        String::new(),
        format!("     {}", bold("-r")),
        "             Like -c, but entries are appended to the".to_owned(),
        "             archive.".to_owned(),
        String::new(),
        format!("     {} {}", bold("-f"), bold("format")),
        "             Use the given format for the archive.".to_owned(),
        String::new(),
        format!("     {}", bold("--null")),
        "             Read null-terminated names.".to_owned(),
    ]
    .join("\n");
    let help = parse_man_page("tar", &page);
    let flags: Vec<(&str, &str)> = help
        .flags
        .iter()
        .map(|entry| (entry.name.as_str(), entry.description.as_str()))
        .collect();
    assert_eq!(
        flags,
        [
            ("-c", "Create a new archive containing the specified items."),
            ("-r", "Like -c, but entries are appended to the archive."),
            ("-f", "Use the given format for the archive."),
            ("--null", "Read null-terminated names."),
        ]
    );
    assert!(!help.flags[0].takes_value);
    assert!(help.flags[2].takes_value);
    assert!(help.subcommands.is_empty());
}

#[test]
fn parses_comma_joined_and_equals_style_flags() {
    let page = format!(
        "{}\n     {}, {}\n             Use the archive suffix.\n\n     {}=_\n             Colorize output.\n",
        bold("OPTIONS"),
        bold("-a"),
        bold("--auto-compress"),
        bold("--color"),
    );
    let help = parse_man_page("ls", &page);
    let names: Vec<&str> = help.flags.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["-a", "--auto-compress", "--color"]);
    assert_eq!(help.flags[0].description, "Use the archive suffix.");
    assert_eq!(help.flags[2].description, "Colorize output.");
}

#[test]
fn parses_git_style_subcommands_only_inside_command_sections() {
    let page = [
        bold("NAME"),
        format!("     {} - the stupid content tracker", bold("git")),
        String::new(),
        bold("GIT COMMANDS"),
        "     We divide Git into groups.".to_owned(),
        String::new(),
        "   Main porcelain commands".to_owned(),
        format!("     {}(1)", bold("git-add")),
        "         Add file contents to the index.".to_owned(),
        String::new(),
        format!("     {}(1)", bold("git-commit")),
        "         Record changes to the repository.".to_owned(),
        String::new(),
        bold("SEE ALSO"),
        format!("     {}(1)", bold("git-web--browse")),
        "         Not a subcommand section.".to_owned(),
    ]
    .join("\n");
    let help = parse_man_page("git", &page);
    let names: Vec<&str> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["add", "commit"]);
    assert_eq!(
        help.subcommands[0].description,
        "Add file contents to the index."
    );
}

#[test]
fn parses_two_column_subcommand_tables() {
    let page = [
        bold("NAME"),
        "  demo - demo tool".to_owned(),
        String::new(),
        bold("COMMANDS"),
        "  start   Start the service.".to_owned(),
        "  stop    Stop the service.".to_owned(),
        "  list".to_owned(),
        String::new(),
        bold("EXIT STATUS"),
        "  text here".to_owned(),
    ]
    .join("\n");
    let help = parse_man_page("demo", &page);
    let names: Vec<&str> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["start", "stop"]);
    assert_eq!(help.subcommands[1].description, "Stop the service.");
}

#[test]
fn parses_man_page_command_signatures_with_block_descriptions() {
    let page = [
        bold("COMMANDS"),
        "   install formula|cask...".to_owned(),
        "     Install a formula or cask.".to_owned(),
        String::new(),
        "   update".to_owned(),
        "     Fetch the newest package metadata.".to_owned(),
        String::new(),
        "   doctor, dr [--list-checks]".to_owned(),
        "     Check the system for potential problems.".to_owned(),
        String::new(),
        "     descriptive prose continues here".to_owned(),
    ]
    .join("\n");
    let help = parse_man_page("brew", &page);
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["install", "update", "doctor"]);
    assert_eq!(help.subcommand_aliases, ["dr"]);
    assert_eq!(
        help.subcommands[0].description,
        "formula|cask...: Install a formula or cask."
    );
}

#[test]
fn command_named_non_subcommand_sections_do_not_leak_rows() {
    let page = [
        bold("COMMAND LINE OPTIONS"),
        "  output   This is prose laid out in two columns.".to_owned(),
        bold("COMMAND EXECUTION"),
        "  worker   This is also not a subcommand.".to_owned(),
        bold("SUBCOMMANDS"),
        "  deploy   Ship the service.".to_owned(),
    ]
    .join("\n");
    let help = parse_man_page("demo", &page);
    let names: Vec<_> = help
        .subcommands
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["deploy"]);
}

#[test]
fn garbage_input_yields_no_entries() {
    assert_eq!(parse_man_page("x", ""), CommandHelp::default());
    assert_eq!(
        parse_man_page("x", "random prose\n- not a flag line\n--\n-@notaflag\n"),
        CommandHelp::default()
    );
}

#[test]
fn descriptions_are_collapsed_and_truncated() {
    assert_eq!(shorten("  a   b\tc\n"), "a b c");
    let long = "word ".repeat(40);
    let shortened = shorten(&long);
    assert!(shortened.ends_with('…'));
    assert!(shortened.chars().count() <= MAX_DESCRIPTION_CHARS + 1);
}

#[test]
fn overstrike_markup_collapses_to_visible_glyphs() {
    assert_eq!(strip_overstrike(&bold("NAME")), "NAME");
    assert_eq!(strip_overstrike("_\u{8}f_\u{8}i_\u{8}l_\u{8}e"), "file");
    assert_eq!(strip_overstrike("plain"), "plain");
}
