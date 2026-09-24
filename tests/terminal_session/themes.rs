use super::*;

#[test]
fn real_session_keeps_overlay_when_precmd_rewrites_prompt() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_dynamic_prompt();
    terminal.wait_for_screen("DYN> ");
    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("DYN> ls"));
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn real_session_follows_zdotdir_set_by_user_zshenv() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_user_zdotdir();
    terminal.wait_for_screen("ZDOT> ");
    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("ZDOT> ls"));
    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn powerline_pua_glyphs_pass_through_and_overlay_still_anchors() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_pua_prompt();
    terminal.wait_for_screen("\u{e0b0}\u{f000} HK> ");
    terminal.settle(Duration::from_millis(300));
    let glyphs = "\u{e0b0}\u{f000}".as_bytes();
    assert!(
        terminal
            .transcript
            .windows(glyphs.len())
            .any(|window| window == glyphs),
        "Nerd Font PUA glyphs were not passed through byte-identically"
    );

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("\u{e0b0}\u{f000} HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn rotating_rprompt_redraws_stay_consistent_and_overlay_still_anchors() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_rotating_rprompt();
    terminal.wait_for_screen("RIGHT2");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"true\r");
    terminal.wait_for_screen("RIGHT3");
    terminal.settle(Duration::from_millis(300));
    let text = terminal.screen_text();
    assert!(
        !text.contains("RIGHT2"),
        "stale RPROMPT survived a redraw:\n{text}"
    );
    let prompt_row = *terminal
        .screen_rows_containing("HK> ")
        .last()
        .expect("current prompt row");
    let rprompt_row = *terminal
        .screen_rows_containing("RIGHT3")
        .last()
        .expect("current RPROMPT row");
    assert_eq!(
        rprompt_row, prompt_row,
        "RPROMPT must stay on the current prompt row"
    );
    assert!(
        terminal
            .screen_line(prompt_row)
            .trim_end()
            .ends_with("RIGHT3"),
        "RPROMPT must stay right-aligned on the current prompt row:\n{}",
        terminal.screen_text()
    );

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn multiline_prompt_anchors_overlay_below_the_edit_line() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_multiline_prompt();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    assert!(terminal.screen_text().contains("META"));

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    let edit_row = *terminal
        .screen_rows_containing("HK> ls")
        .last()
        .expect("edit line row");
    let meta_row = *terminal
        .screen_rows_containing("META")
        .last()
        .expect("first prompt line row");
    assert_eq!(
        meta_row + 1,
        edit_row,
        "META must sit directly above the edit line"
    );
    let overlay_row = edit_row + 1;
    assert!(
        terminal.screen_line(overlay_row).contains('╭'),
        "overlay top border must anchor below the edit line, not below the first prompt line:\n{}",
        terminal.screen_text()
    );
    assert!(
        terminal.screen_line(overlay_row + 1).contains(TAG_SPEC),
        "overlay items must sit below the top border:\n{}",
        terminal.screen_text()
    );
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn instant_prompt_churn_passes_through_and_overlay_works_at_first_prompt() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_instant_prompt_churn();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));
    assert!(
        terminal
            .transcript
            .windows(b"cached segment row".len())
            .any(|window| window == b"cached segment row"),
        "instant prompt churn did not pass through to the outer terminal"
    );
    let text = terminal.screen_text();
    assert!(
        !text.contains("CACHED") && !text.contains("cached segment"),
        "cached prompt block was not erased before the real prompt:\n{text}"
    );

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    assert!(terminal.screen_text().contains("HK> ls"));
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}

#[test]
fn transient_prompt_rewrite_reanchors_overlay_on_return_to_prompt() {
    if !command_exists("zsh") {
        return;
    }
    let mut terminal = TerminalSession::spawn_with_transient_prompt();
    terminal.wait_for_screen("HK> ");
    terminal.settle(Duration::from_millis(300));

    terminal.write(b"true\r");
    terminal.wait_for_screen("TR> true");
    terminal.settle(Duration::from_millis(300));
    let transient_row = *terminal
        .screen_rows_containing("TR> true")
        .last()
        .expect("transient prompt row");
    let prompt_row = *terminal
        .screen_rows_containing("HK> ")
        .last()
        .expect("fresh prompt row");
    assert!(
        prompt_row > transient_row,
        "a fresh prompt must follow the transient rewrite:\n{}",
        terminal.screen_text()
    );

    terminal.write(b"ls ");
    terminal.wait_for_screen(TAG_SPEC);
    let edit_row = *terminal
        .screen_rows_containing("HK> ls")
        .last()
        .expect("edit line row");
    let overlay_row = edit_row + 1;
    assert!(
        terminal.screen_line(overlay_row).contains('╭'),
        "overlay top border must re-anchor below the new edit line:\n{}",
        terminal.screen_text()
    );
    assert!(
        terminal.screen_line(overlay_row + 1).contains(TAG_SPEC),
        "overlay items must sit below the top border:\n{}",
        terminal.screen_text()
    );
    assert_forbidden_overlay_sequences_absent(&terminal.transcript);

    terminal.exit_shell();
    terminal.wait_until_exit();
}
