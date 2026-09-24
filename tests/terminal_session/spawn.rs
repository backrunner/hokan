use super::*;

impl TerminalSession {
    pub(super) fn spawn() -> Self {
        Self::spawn_with_sync_status(2)
    }

    pub(super) fn spawn_with_sync_status(sync_status: u8) -> Self {
        let (home, work) = fixture_directories();
        let mut terminal = Self::spawn_hokan(home, work, sync_status);
        // Ordinary interaction tests start after the terminal handshake.
        // Startup handoff tests use spawn_hokan directly to queue early input.
        // A fixed sleep can expire before this test thread answers a probe
        // under parallel load, leaking a late response into its next command.
        terminal.wait_for_sync_replies(1);
        terminal
    }

    pub(super) fn spawn_without_private_cpr() -> Self {
        let (home, work) = fixture_directories();
        let mut command = CommandBuilder::new(hokan_test_bin());
        command.arg("--shell");
        command.arg("zsh");
        configure_command(&mut command, &home, &work);
        Self::spawn_command_with_private_cpr(home, work, command, 2, false)
    }

    pub(super) fn spawn_with_dynamic_prompt() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT='BASE> '\nRPROMPT=''\nsetopt no_beep\n\
             autoload -Uz add-zsh-hook\n\
             function fixture_dynamic_prompt() { PROMPT='DYN> ' }\n\
             add-zsh-hook precmd fixture_dynamic_prompt\n",
        )
        .expect("dynamic prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    pub(super) fn spawn_with_user_zdotdir() -> Self {
        let (home, work) = empty_fixture_directories();
        let user_zdotdir = home.path().join("custom-zdotdir");
        fs::create_dir(&user_zdotdir).expect("custom ZDOTDIR");
        fs::write(
            home.path().join(".zshenv"),
            "unsetopt GLOBAL_RCS\nexport ZDOTDIR=\"$HOME/custom-zdotdir\"\n",
        )
        .expect("zshenv fixture");
        fs::write(
            user_zdotdir.join(".zshrc"),
            "PROMPT='ZDOT> '\nRPROMPT=''\nsetopt no_beep\n",
        )
        .expect("custom zshrc fixture");
        Self::spawn_hokan(home, work, 2)
    }

    pub(super) fn spawn_with_pua_prompt() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT=$'\u{e0b0}\u{f000} HK> '\nRPROMPT=''\nsetopt no_beep\n",
        )
        .expect("PUA prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    pub(super) fn spawn_with_rotating_rprompt() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT='HK> '\nRPROMPT='RIGHT1'\nsetopt no_beep\n\
             setopt transient_rprompt\n\
             typeset -gi FIXTURE_RPROMPT_N=1\n\
             autoload -Uz add-zsh-hook\n\
             function fixture_rotate_rprompt() {\n\
             \x20 (( FIXTURE_RPROMPT_N++ ))\n\
             \x20 RPROMPT=\"RIGHT${FIXTURE_RPROMPT_N}\"\n\
             }\n\
             add-zsh-hook precmd fixture_rotate_rprompt\n",
        )
        .expect("rotating RPROMPT fixture");
        Self::spawn_hokan(home, work, 2)
    }

    pub(super) fn spawn_with_multiline_prompt() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT=$'META\\nHK> '\nRPROMPT=''\nsetopt no_beep\n",
        )
        .expect("multiline prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    pub(super) fn spawn_with_instant_prompt_churn() -> Self {
        let (home, work) = empty_fixture_directories();
        // Emulates p10k instant prompt: a cached prompt block is printed and
        // then erased at the very top of .zshrc, exactly where p10k runs it,
        // before hokan's init hook is sourced.
        fs::write(
            home.path().join(".zshrc"),
            "print -r -- 'CACHED> '\n\
             print -r -- 'cached segment row'\n\
             print -rn -- $'\\e[2A\\r\\e[J'\n\
             PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n",
        )
        .expect("instant prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    pub(super) fn spawn_with_transient_prompt() -> Self {
        let (home, work) = empty_fixture_directories();
        // Emulates p10k transient prompt: on preexec the accepted line is
        // rewritten to a shorter prompt while the command runs.
        fs::write(
            home.path().join(".zshrc"),
            "PROMPT='HK> '\nRPROMPT=''\nsetopt no_beep\n\
             autoload -Uz add-zsh-hook\n\
             function fixture_transient_prompt() {\n\
             \x20 print -rn -- $'\\e[1A\\r\\e[KTR> '\n\
             \x20 print -r -- \"$1\"\n\
             }\n\
             add-zsh-hook preexec fixture_transient_prompt\n",
        )
        .expect("transient prompt fixture");
        Self::spawn_hokan(home, work, 2)
    }

    pub(super) fn spawn_via_zsh_install() -> Self {
        let (home, work) = fixture_directories();
        let rc_path = home.path().join(".zshrc");
        fs::write(
            &rc_path,
            "typeset -gi HOKAN_RC_LOADS=${HOKAN_RC_LOADS:-0}\n\
             (( HOKAN_RC_LOADS++ ))\n\
             export HOKAN_RC_LOADS\n\
             PROMPT=\"HK${HOKAN_RC_LOADS}> \"\n\
             RPROMPT=''\n\
             setopt no_beep\n",
        )
        .expect("auto-start zshrc fixture");
        let output = Command::new(hokan_test_bin())
            .arg("--shell")
            .arg("zsh")
            .arg("install")
            .arg("--rc-file")
            .arg(&rc_path)
            .env("HOME", home.path())
            .env_remove("HOKAN_ACTIVE")
            .env_remove("HOKAN_BIN")
            .output()
            .expect("run hokan install");
        assert!(
            output.status.success(),
            "install failed: stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let installed = fs::read_to_string(&rc_path).expect("installed zshrc");
        assert!(installed.contains("HOKAN_AUTO_START"));

        let mut command = CommandBuilder::new("zsh");
        command.arg("-i");
        configure_command(&mut command, &home, &work);
        Self::spawn_command(home, work, command, 2)
    }

    pub(super) fn spawn_hokan(home: TempDir, work: TempDir, sync_status: u8) -> Self {
        let mut command = CommandBuilder::new(hokan_test_bin());
        command.arg("--shell");
        command.arg("zsh");
        configure_command(&mut command, &home, &work);
        Self::spawn_command(home, work, command, sync_status)
    }

    pub(super) fn spawn_bash() -> Self {
        let (home, work) = empty_fixture_directories();
        fs::write(home.path().join(".bashrc"), "PS1='BASH> '\n").expect("fixture bashrc");
        let mut command = CommandBuilder::new(hokan_test_bin());
        command.arg("--shell");
        command.arg("bash");
        configure_command(&mut command, &home, &work);
        command.env("SHELL", "/bin/bash");
        Self::spawn_command(home, work, command, 2)
    }

    pub(super) fn spawn_termios_wrapper() -> Self {
        let (home, work) = fixture_directories();
        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg(
            "before=$(stty -g) || exit 90\n\
             \"$HOKAN_TEST_BIN\" --shell zsh\n\
             status=$?\n\
             after=$(stty -g) || exit 91\n\
             if [ \"$before\" = \"$after\" ]; then restored=yes; else restored=no; fi\n\
             printf '\\r\\nHK_TERMIO:%s:EXIT=%s\\r\\n' \"$restored\" \"$status\"",
        );
        command.env("HOKAN_TEST_BIN", hokan_test_bin());
        configure_command(&mut command, &home, &work);
        Self::spawn_command(home, work, command, 2)
    }

    pub(super) fn spawn_in_tmux() -> Self {
        let (home, work) = fixture_directories();
        let tmux_config = home.path().join("tmux.conf");
        fs::write(
            &tmux_config,
            "set -g status off\nset -g destroy-unattached on\nset -g exit-empty on\n",
        )
        .expect("tmux fixture config");
        let socket = format!(
            "hokan-test-{}-{}",
            std::process::id(),
            TMUX_SOCKET_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let mut command = CommandBuilder::new("tmux");
        command.arg("-L");
        command.arg(socket);
        command.arg("-f");
        command.arg(tmux_config);
        command.arg("new-session");
        command.arg("-x");
        command.arg("80");
        command.arg("-y");
        command.arg("24");
        command.arg(hokan_test_bin());
        command.arg("--shell");
        command.arg("zsh");
        configure_command(&mut command, &home, &work);
        command.env_remove("TMUX");
        command.env_remove("TMUX_PANE");
        Self::spawn_command(home, work, command, 2)
    }

    pub(super) fn spawn_command(
        home: TempDir,
        work: TempDir,
        command: CommandBuilder,
        sync_status: u8,
    ) -> Self {
        Self::spawn_command_with_private_cpr(home, work, command, sync_status, true)
    }

    /// A colored session (no `NO_COLOR`) with scrollback captured in the
    /// fixture parser, for residue checks that need to inspect rows that
    /// scrolled off the top of the screen.
    pub(super) fn spawn_colored_with_scrollback(scrollback: usize) -> Self {
        let (home, work) = fixture_directories();
        let mut command = CommandBuilder::new(hokan_test_bin());
        command.arg("--shell");
        command.arg("zsh");
        configure_command(&mut command, &home, &work);
        command.env_remove("NO_COLOR");
        Self::spawn_command_with_private_cpr_and_scrollback(
            home, work, command, 2, true, scrollback,
        )
    }

    pub(super) fn spawn_command_with_private_cpr(
        home: TempDir,
        work: TempDir,
        command: CommandBuilder,
        sync_status: u8,
        private_cpr_supported: bool,
    ) -> Self {
        Self::spawn_command_with_private_cpr_and_scrollback(
            home,
            work,
            command,
            sync_status,
            private_cpr_supported,
            0,
        )
    }

    pub(super) fn spawn_command_with_private_cpr_and_scrollback(
        home: TempDir,
        work: TempDir,
        command: CommandBuilder,
        sync_status: u8,
        private_cpr_supported: bool,
        scrollback: usize,
    ) -> Self {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("outer PTY");
        let child = pair.slave.spawn_command(command).expect("hokan child");
        drop(pair.slave);
        let reader = pair.master.try_clone_reader().expect("PTY reader");
        let writer = pair.master.take_writer().expect("PTY writer");
        let (sender, chunks) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = reader;
            let mut bytes = [0_u8; 16 * 1024];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        if sender.send(bytes[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        continue;
                    }
                    Err(error) if error.raw_os_error() == Some(5) => break,
                    Err(_) => break,
                }
            }
        });

        Self {
            _home: home,
            _work: work,
            master: Some(pair.master),
            writer: Some(writer),
            child,
            chunks,
            reader: Some(reader),
            terminal: vt100::Parser::new(24, 80, scrollback),
            rows: 24,
            cols: 80,
            transcript: Vec::new(),
            probe_tail: Vec::new(),
            sync_status,
            private_cpr_supported,
            sync_replies: 0,
            cpr_replies: 0,
        }
    }
}
