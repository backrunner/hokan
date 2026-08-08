use super::ShellKind;

pub const PROTOCOL_VERSION: u8 = 2;

#[must_use]
pub fn init_script(shell: ShellKind) -> String {
    match shell {
        ShellKind::Zsh => zsh_script(),
        ShellKind::Bash => bash_script(),
        ShellKind::Fish => fish_script(),
    }
}

fn zsh_script() -> String {
    r#"# hokan shell integration protocol 2
if [[ -n ${HOKAN_ACTIVE:-} && -n ${HOKAN_CONTROL_FIFO:-} && -z ${__HOKAN_ZSH_LOADED:-} \
      && ( -z ${HOKAN_HOOK_OWNER_PID:-} || $HOKAN_HOOK_OWNER_PID == $$ ) ]]; then
  typeset -gx HOKAN_HOOK_OWNER_PID=$$
  typeset -g __HOKAN_ZSH_LOADED=1
  typeset -gi __hokan_prompt_id=0
  typeset -gi __hokan_redisplay_id=0
  typeset -gi __hokan_command_active=0
  typeset -g __hokan_last_command=''
  typeset -g __hokan_last_path=''
  typeset -g __hokan_prompt_base="$PROMPT"
  typeset -g __hokan_wrapped_prompt=''
  exec {__hokan_control_fd}>"$HOKAN_CONTROL_FIFO"

  function __hokan_prompt_marker() {
    printf '\033]6973;hokan;1;%s;prompt;%d;%s\033\\' \
      "$HOKAN_SESSION_TOKEN" "$__hokan_prompt_id" "$HOKAN_PROMPT_CRC"
  }

  function __hokan_redisplay_marker() {
    printf '\033]6973;hokan;1;%s;redisplay;%d;%s\033\\' \
      "$HOKAN_SESSION_TOKEN" "$__hokan_redisplay_id" "$HOKAN_REDISPLAY_CRC"
  }

  function __hokan_refresh_prompt() {
    if [[ "$PROMPT" != "$__hokan_wrapped_prompt" ]]; then
      __hokan_prompt_base="$PROMPT"
    fi
    # Embed the marker literally: themes like oh-my-posh unset PROMPT_SUBST in
    # their own precmd, so a deferred `$(...)` would render as literal text.
    # The id only advances per precmd, so repeated prints on redisplay are
    # identical to what deferred evaluation produced.
    local __hokan_marker
    __hokan_marker="$(__hokan_prompt_marker)"
    __hokan_wrapped_prompt="${__hokan_prompt_base}%{${__hokan_marker}%}"
    PROMPT="$__hokan_wrapped_prompt"
  }

  function __hokan_sync_path() {
    if [[ "$PATH" != "$__hokan_last_path" ]]; then
      printf 'HKP2\tPATH\t%s\0' "$PATH" >&$__hokan_control_fd
      __hokan_last_path="$PATH"
    fi
  }

  function __hokan_precmd() {
    local command_status=$?
    if (( __hokan_command_active )); then
      printf 'HKP2\tEND\t%d\t%s\0' "$command_status" "$PWD" \
        >&$__hokan_control_fd
      __hokan_command_active=0
    fi
    __hokan_sync_path
    (( __hokan_prompt_id++ ))
    printf 'HKP2\tPROMPT\t%d\t%s\0' "$__hokan_prompt_id" "$PWD" \
      >&$__hokan_control_fd
    __hokan_refresh_prompt
  }

  function __hokan_preexec() {
    __hokan_last_command="$1"
    __hokan_command_active=1
    printf 'HKP2\tSTART\t%s\0' "$1" >&$__hokan_control_fd
  }

  function __hokan_line_pre_redraw() {
    # Themes with async rendering (oh-my-posh) can overwrite PROMPT after the
    # precmd chain; re-assert the wrapper. The guard keeps this recursion-free.
    if [[ "$PROMPT" != "$__hokan_wrapped_prompt" ]]; then
      __hokan_refresh_prompt
    fi
    (( __hokan_redisplay_id++ ))
    printf 'HKP2\tBUFFER\t%d\t%d\t%s\0' "$__hokan_redisplay_id" "$CURSOR" \
      "$BUFFER" >&$__hokan_control_fd
    # This marker begins a redraw. Hokan waits for the following PTY EAGAIN
    # boundary before treating the matching buffer snapshot as visible.
    __hokan_redisplay_marker
  }

  function __hokan_apply() {
    local edit_payload next_cursor next_buffer
    edit_payload="$("${HOKAN_BIN:-hokan}" ipc take --session "$HOKAN_SESSION_TOKEN")" \
      || return 0
    next_cursor=${edit_payload%%$'\t'*}
    next_buffer=${edit_payload#*$'\t'}
    [[ "$next_cursor" == <-> ]] || return 0
    BUFFER="$next_buffer"
    CURSOR=$next_cursor
    zle redisplay
  }

  function __hokan_apply_accept() {
    local edit_payload next_cursor next_buffer
    edit_payload="$("${HOKAN_BIN:-hokan}" ipc take --session "$HOKAN_SESSION_TOKEN")" \
      || return 0
    next_cursor=${edit_payload%%$'\t'*}
    next_buffer=${edit_payload#*$'\t'}
    [[ "$next_cursor" == <-> ]] || return 0
    BUFFER="$next_buffer"
    CURSOR=$next_cursor
    zle accept-line
  }

  autoload -Uz add-zsh-hook add-zle-hook-widget
  add-zsh-hook precmd __hokan_precmd
  add-zsh-hook preexec __hokan_preexec
  add-zle-hook-widget line-pre-redraw __hokan_line_pre_redraw
  zle -N __hokan_apply
  bindkey '\e[99~' __hokan_apply
  zle -N __hokan_apply_accept
  bindkey '\e[98~' __hokan_apply_accept
  setopt prompt_subst
  __hokan_refresh_prompt
fi
"#
    .to_owned()
}

fn bash_script() -> String {
    r#"# hokan shell integration protocol 2
if [[ -n ${HOKAN_ACTIVE:-} && -n ${HOKAN_CONTROL_FIFO:-} && -z ${__HOKAN_BASH_LOADED:-} \
      && ( -z ${HOKAN_HOOK_OWNER_PID:-} || $HOKAN_HOOK_OWNER_PID == $$ ) ]]; then
  export HOKAN_HOOK_OWNER_PID=$$
  __HOKAN_BASH_LOADED=1
  __hokan_prompt_id=0
  __hokan_last_status=0
  __hokan_last_history=$(HISTTIMEFORMAT= builtin history 1)
  __hokan_last_path=''
  exec 9>"$HOKAN_CONTROL_FIFO"

  __hokan_sync_path() {
    if [[ "$PATH" != "$__hokan_last_path" ]]; then
      printf 'HKP2\tPATH\t%s\0' "$PATH" >&9
      __hokan_last_path="$PATH"
    fi
  }

  __hokan_prompt_command() {
    local current_history
    local last_command
    current_history=$(HISTTIMEFORMAT= builtin history 1)
    if [[ -n "$current_history" && "$current_history" != "$__hokan_last_history" ]]; then
      last_command=$current_history
      last_command=${last_command#"${last_command%%[![:space:]]*}"}
      last_command=${last_command#* }
      last_command=${last_command#"${last_command%%[![:space:]]*}"}
      printf 'HKP2\tSTART\t%s\0' "$last_command" >&9
      printf 'HKP2\tEND\t%d\t%s\0' "$__hokan_last_status" "$PWD" >&9
      __hokan_last_history=$current_history
    fi
    __hokan_sync_path
    __hokan_prompt_id=$((__hokan_prompt_id + 1))
    printf 'HKP2\tPROMPT\t%d\t%s\t%s\0' "$__hokan_prompt_id" \
      "${HISTCONTROL:-}" "$PWD" >&9
  }

  __hokan_restore_status() {
    return "$__hokan_last_status"
  }

  __hokan_prompt_marker() {
    printf '\033]6973;hokan;1;%s;prompt;%d;%s\033\\' \
      "$HOKAN_SESSION_TOKEN" "$__hokan_prompt_id" "$HOKAN_PROMPT_CRC"
  }

  __hokan_apply() {
    local edit_payload next_cursor next_buffer
    edit_payload="$("${HOKAN_BIN:-hokan}" ipc take --session "$HOKAN_SESSION_TOKEN")" \
      || return 0
    next_cursor=${edit_payload%%$'\t'*}
    next_buffer=${edit_payload#*$'\t'}
    [[ "$next_cursor" =~ ^[0-9]+$ ]] || return 0
    READLINE_LINE="$next_buffer"
    READLINE_POINT=$next_cursor
  }

  bind -x '"\C-x\C-]":__hokan_apply'
  __hokan_original_prompt_command=${PROMPT_COMMAND:-}
  PROMPT_COMMAND='__hokan_last_status=$?; __hokan_restore_status;'
  if [[ -n $__hokan_original_prompt_command ]]; then
    PROMPT_COMMAND+=" $__hokan_original_prompt_command;"
  fi
  PROMPT_COMMAND+=' __hokan_prompt_command'
  PS1="${PS1}"'\[$(__hokan_prompt_marker)\]'
fi
"#
    .to_owned()
}

fn fish_script() -> String {
    r#"# hokan shell integration protocol 2
if test -n "$HOKAN_ACTIVE"; and test -n "$HOKAN_CONTROL_FIFO"; \
    and not set -q __HOKAN_FISH_LOADED; \
    and begin; test -z "$HOKAN_HOOK_OWNER_PID"; \
      or test "$HOKAN_HOOK_OWNER_PID" = "$fish_pid"; end
  set -gx HOKAN_HOOK_OWNER_PID $fish_pid
  set -g __HOKAN_FISH_LOADED 1
  set -g __hokan_prompt_id 0
  set -g __hokan_last_path ''

  function __hokan_sync_path
    set -l current_path (string join : -- $PATH)
    if test "$current_path" != "$__hokan_last_path"
      printf 'HKP2\tPATH\t%s\0' "$current_path" >$HOKAN_CONTROL_FIFO
      set -g __hokan_last_path "$current_path"
    end
  end

  function __hokan_emit_prompt
    printf 'HKP2\tPROMPT\t%d\t%s\0' $__hokan_prompt_id "$PWD" >$HOKAN_CONTROL_FIFO
  end

  function __hokan_preexec --on-event fish_preexec
    printf 'HKP2\tSTART\t%s\0' "$argv[1]" >$HOKAN_CONTROL_FIFO
  end

  function __hokan_postexec --on-event fish_postexec
    set -l command_status $status
    printf 'HKP2\tEND\t%d\t%s\0' $command_status "$PWD" \
      >$HOKAN_CONTROL_FIFO
  end

  function __hokan_apply
    set -l edit_payload (command "$HOKAN_BIN" ipc take --session "$HOKAN_SESSION_TOKEN")
    or return
    set -l edit_fields (string split -m 1 \t -- "$edit_payload")
    test (count $edit_fields) -eq 2; or return
    string match -qr '^[0-9]+$' -- "$edit_fields[1]"; or return
    set -l next_cursor $edit_fields[1]
    set -l next_buffer $edit_fields[2]
    commandline --replace -- "$next_buffer"
    commandline --cursor $next_cursor
    commandline -f repaint
  end

  functions -c fish_prompt __hokan_original_fish_prompt
  function fish_prompt
    set -g __hokan_prompt_id (math $__hokan_prompt_id + 1)
    __hokan_sync_path
    __hokan_emit_prompt
    __hokan_original_fish_prompt
    printf '\033]6973;hokan;1;%s;prompt;%d;%s\033\\' \
      "$HOKAN_SESSION_TOKEN" $__hokan_prompt_id "$HOKAN_PROMPT_CRC"
  end

  bind \e\[99\~ __hokan_apply
  bind -M insert \e\[99\~ __hokan_apply
end
"#
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_are_versioned_idempotent_and_native() {
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            let script = init_script(shell);
            assert!(script.contains("protocol 2"));
            assert!(script.contains("HKP2\\tPATH\\t%s\\0"));
            assert!(script.contains("HOKAN_ACTIVE"));
            assert!(script.contains("ipc take"));
            assert!(script.contains("HKP2"));
            assert!(script.contains("6973;hokan;1"));
        }
        assert!(init_script(ShellKind::Zsh).contains("BUFFER="));
        assert!(init_script(ShellKind::Zsh).contains("__hokan_refresh_prompt"));
        assert!(init_script(ShellKind::Zsh).contains("__hokan_apply_accept"));
        assert!(init_script(ShellKind::Zsh).contains("zle accept-line"));
        assert!(init_script(ShellKind::Zsh).contains("bindkey '\\e[98~'"));
        assert!(init_script(ShellKind::Bash).contains("READLINE_LINE="));
        assert!(init_script(ShellKind::Fish).contains("commandline --replace"));
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            assert!(init_script(shell).contains("HOKAN_HOOK_OWNER_PID"));
        }
    }

    #[test]
    fn integration_scripts_do_not_install_arrow_bindings() {
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            let script = init_script(shell);
            for sequence in [r"\e[A", r"\e[B", r"\eOA", r"\eOB", r"\e\[A", r"\e\[B"] {
                assert!(
                    !script.contains(sequence),
                    "{shell} integration must not bind {sequence}"
                );
            }
        }
    }
}
