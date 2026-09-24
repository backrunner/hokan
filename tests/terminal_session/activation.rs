use super::*;

#[test]
fn enter_executes_typed_command_with_one_press_while_overlay_is_open() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    // Seed hokan's history so the candidate list opens while the real command
    // is typed below.
    terminal.write(b"echo HI_DONE_SEED\r");
    terminal.wait_for_screen("HI_DONE_SEED");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"echo HI_DONE");
    terminal.wait_for_screen(TAG_HIS);
    assert!(terminal.screen_text().contains("HK> echo HI_DONE"));

    // ONE Enter must execute exactly what was typed — with no explicit
    // selection it never touches the candidate list or the buffer.
    terminal.write(b"\r");
    terminal.wait_for_bare_row("HI_DONE");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn enter_runs_ls_with_one_press_while_overlay_is_open() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    fs::write(terminal._work.path().join("HKLS_MARKER.txt"), b"marker\n").expect("marker file");
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    // ONE Enter runs the typed command: nothing is selected by default, so
    // Enter passes the buffer through to the shell unchanged.
    terminal.write(b"\r");
    terminal.wait_for_screen("HKLS_MARKER.txt");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn tab_fill_clears_zsh_inline_suggestion_before_further_typing() {
    if !command_exists("zsh") {
        return;
    }
    let (home, work) = fixture_directories();
    // Model a suggestion plugin that owns POSTDISPLAY. ZLE keeps this suffix
    // when a custom widget changes BUFFER unless the widget clears it.
    fs::write(
        home.path().join(".zshrc"),
        "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\nbindkey -e\n\
         function fixture_inline_suggestion() {\n\
           BUFFER='tar '; CURSOR=${#BUFFER}\n\
           POSTDISPLAY='HK_STALE_SUGGESTION'\n\
           region_highlight+=(\"$CURSOR $(( CURSOR + ${#POSTDISPLAY} )) fg=8\")\n\
         }\n\
         zle -N fixture_inline_suggestion\n\
         bindkey '^G' fixture_inline_suggestion\n",
    )
    .expect("inline suggestion fixture");
    let mut terminal = TerminalSession::spawn_hokan(home, work, 2);
    terminal.wait_for_screen("HK> ");
    terminal.wait_for_sync_replies(1);
    terminal.write(b"\x07");
    terminal.wait_for_screen("HK> tar HK_STALE_SUGGESTION");
    terminal.wait_for_screen(TAG_SPEC);
    terminal.write(b"\t");
    terminal.wait_for_screen("HK> tar -czf");
    terminal.settle(Duration::from_millis(200));
    assert!(
        !terminal.screen_text().contains("HK_STALE_SUGGESTION"),
        "old inline suggestion survived Tab:\n{}",
        terminal.screen_text()
    );
    terminal.write(b"archive");
    terminal.wait_for_screen("HK> tar -czf archive");
    terminal.settle(Duration::from_millis(200));
    assert!(
        !terminal.screen_text().contains("HK_STALE_SUGGESTION"),
        "typing pushed the old inline suggestion forward:\n{}",
        terminal.screen_text()
    );
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn tab_fills_back_the_selected_candidate_without_executing() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"tar ");
    terminal.wait_for_screen(TAG_SPEC);
    // No row is pre-selected: Down selects the first candidate, then Tab —
    // the fill edit-back path — rewrites the buffer to the candidate text…
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");
    terminal.write(b"\t");
    terminal.wait_for_screen("HK> tar -czf");
    terminal.settle(Duration::from_millis(300));
    // …but nothing was executed — the line is still being edited.
    let text = terminal.screen_text();
    assert!(!text.contains("tar: "), "Tab must not execute:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn tab_without_a_selection_fills_the_top_candidate_and_selects_first_row() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    fs::create_dir(terminal._work.path().join("archive-dir"))
        .expect("archive navigation directory");
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"tar ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(
        !terminal.screen_text().contains('▶'),
        "no row may be pre-selected:\n{}",
        terminal.screen_text()
    );

    // Tab with no explicit selection fills the top-ranked candidate…
    terminal.write(b"\t");
    terminal.wait_for_screen("HK> tar -czf");
    // …and the refreshed list selects its first row automatically.
    terminal.wait_for_screen("▶");
    terminal.settle(Duration::from_millis(300));
    let text = terminal.screen_text();
    assert!(!text.contains("tar: "), "Tab must not execute:\n{text}");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn cd_completion_enter_executes_by_default() {
    check_cd_completion_key(None, b"\r", true);
}

#[test]
fn cd_completion_enter_can_continue_to_child_directories() {
    check_cd_completion_key(Some("continue"), b"\r", false);
}

#[test]
fn cd_completion_tab_still_continues_without_executing() {
    check_cd_completion_key(None, b"\t", false);
}

#[test]
fn enter_executes_the_selected_history_candidate() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    // Seed a unique history entry whose OUTPUT differs from its command text
    // (lowercase) so execution is distinguishable from overlay echoes.
    terminal.write(b"echo HKSEL_HIDDEN | tr A-Z a-z\r");
    terminal.wait_for_bare_row("hksel_hidden");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"echo HKSEL_H");
    terminal.wait_for_screen(TAG_HIS);
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");

    // ONE Enter on the explicit selection executes the candidate outright.
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(start, b"hksel_hidden");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn enter_on_a_dangerous_candidate_requires_confirmation() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    terminal.write(b"rm -rf /tmp/hokan-danger-x && echo HKDANGER_X | tr A-Z a-z\r");
    terminal.wait_for_bare_row("hkdanger_x");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"rm -rf /tmp/hokan-dan");
    terminal.wait_for_screen(TAG_HIS);
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");

    // First Enter only opens the danger confirmation — nothing executes.
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_screen("Enter 确认执行 · Esc 取消");
    let text = terminal.screen_text();
    assert!(text.contains(TAG_EXEC), "EXEC row missing:\n{text}");
    assert!(
        text.contains("rm -rf /tmp/hokan-danger-x"),
        "final command missing from the EXEC row:\n{text}"
    );
    assert!(
        !terminal.transcript[start..]
            .windows(b"hkdanger_x".len())
            .any(|window| window == b"hkdanger_x"),
        "the dangerous command executed before confirmation"
    );

    // Second Enter proceeds with the execution.
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_bytes_since(start, b"hkdanger_x");

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn escape_cancels_the_danger_confirmation() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    terminal.write(b"rm -rf /tmp/hokan-danger-y && echo HKCANCEL_Y | tr A-Z a-z\r");
    terminal.wait_for_bare_row("hkcancel_y");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"rm -rf /tmp/hokan-dan");
    terminal.wait_for_screen(TAG_HIS);
    terminal.write(b"\x1b[B");
    terminal.wait_for_screen("▶");
    let start = terminal.transcript.len();
    terminal.write(b"\r");
    terminal.wait_for_screen("Enter 确认执行 · Esc 取消");

    // Esc drops the confirmation and brings the normal candidate list back.
    terminal.write(b"\x1b");
    terminal.wait_for_screen(TAG_HIS);
    terminal.settle(Duration::from_millis(200));
    let text = terminal.screen_text();
    assert!(
        !text.contains("确认执行"),
        "confirmation hint survived Esc:\n{text}"
    );
    terminal.settle(Duration::from_millis(300));
    assert!(
        !terminal.transcript[start..]
            .windows(b"hkcancel_y".len())
            .any(|window| window == b"hkcancel_y"),
        "the dangerous command executed despite Esc"
    );

    terminal.exit_shell();
    terminal.wait_until_exit();
}
