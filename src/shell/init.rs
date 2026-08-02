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
    r#"# hokann shell integration protocol 2
if [[ -n ${HOKANN_ACTIVE:-} && -n ${HOKANN_CONTROL_FIFO:-} && -z ${__HOKANN_ZSH_LOADED:-} \
      && ( -z ${HOKANN_HOOK_OWNER_PID:-} || $HOKANN_HOOK_OWNER_PID == $$ ) ]]; then
  typeset -gx HOKANN_HOOK_OWNER_PID=$$
  typeset -g __HOKANN_ZSH_LOADED=1
  typeset -gi __hokann_prompt_id=0
  typeset -gi __hokann_redisplay_id=0
  typeset -gi __hokann_command_active=0
  typeset -g __hokann_last_command=''
  typeset -g __hokann_prompt_base="$PROMPT"
  typeset -g __hokann_wrapped_prompt=''
  exec {__hokann_control_fd}>"$HOKANN_CONTROL_FIFO"

  function __hokann_prompt_marker() {
    printf '\033]6973;hokann;1;%s;prompt;%d;%s\033\\' \
      "$HOKANN_SESSION_TOKEN" "$__hokann_prompt_id" "$HOKANN_PROMPT_CRC"
  }

  function __hokann_redisplay_marker() {
    printf '\033]6973;hokann;1;%s;redisplay;%d;%s\033\\' \
      "$HOKANN_SESSION_TOKEN" "$__hokann_redisplay_id" "$HOKANN_REDISPLAY_CRC"
  }

  function __hokann_refresh_prompt() {
    if [[ "$PROMPT" != "$__hokann_wrapped_prompt" ]]; then
      __hokann_prompt_base="$PROMPT"
    fi
    __hokann_wrapped_prompt='${__hokann_prompt_base}%{$(__hokann_prompt_marker)%}'
    PROMPT="$__hokann_wrapped_prompt"
  }

  function __hokann_precmd() {
    local command_status=$?
    if (( __hokann_command_active )); then
      printf 'HKP2\tEND\t%d\t%s\0' "$command_status" "$PWD" \
        >&$__hokann_control_fd
      __hokann_command_active=0
    fi
    (( __hokann_prompt_id++ ))
    printf 'HKP2\tPROMPT\t%d\t%s\0' "$__hokann_prompt_id" "$PWD" \
      >&$__hokann_control_fd
    __hokann_refresh_prompt
  }

  function __hokann_preexec() {
    __hokann_last_command="$1"
    __hokann_command_active=1
    printf 'HKP2\tSTART\t%s\0' "$1" >&$__hokann_control_fd
  }

  function __hokann_line_pre_redraw() {
    (( __hokann_redisplay_id++ ))
    printf 'HKP2\tBUFFER\t%d\t%d\t%s\0' "$__hokann_redisplay_id" "$CURSOR" \
      "$BUFFER" >&$__hokann_control_fd
    # This marker begins a redraw. Hokann waits for the following PTY EAGAIN
    # boundary before treating the matching buffer snapshot as visible.
    __hokann_redisplay_marker
  }

  function __hokann_apply() {
    local edit_payload next_cursor next_buffer
    edit_payload="$("${HOKANN_BIN:-hokann}" ipc take --session "$HOKANN_SESSION_TOKEN")" \
      || return 0
    next_cursor=${edit_payload%%$'\t'*}
    next_buffer=${edit_payload#*$'\t'}
    [[ "$next_cursor" == <-> ]] || return 0
    BUFFER="$next_buffer"
    CURSOR=$next_cursor
    zle redisplay
  }

  autoload -Uz add-zsh-hook add-zle-hook-widget
  add-zsh-hook precmd __hokann_precmd
  add-zsh-hook preexec __hokann_preexec
  add-zle-hook-widget line-pre-redraw __hokann_line_pre_redraw
  zle -N __hokann_apply
  bindkey '\e[99~' __hokann_apply
  setopt prompt_subst
  __hokann_refresh_prompt
fi
"#
    .to_owned()
}

fn bash_script() -> String {
    r#"# hokann shell integration protocol 2
if [[ -n ${HOKANN_ACTIVE:-} && -n ${HOKANN_CONTROL_FIFO:-} && -z ${__HOKANN_BASH_LOADED:-} \
      && ( -z ${HOKANN_HOOK_OWNER_PID:-} || $HOKANN_HOOK_OWNER_PID == $$ ) ]]; then
  export HOKANN_HOOK_OWNER_PID=$$
  __HOKANN_BASH_LOADED=1
  __hokann_prompt_id=0
  __hokann_last_status=0
  __hokann_last_history=$(HISTTIMEFORMAT= builtin history 1)
  exec 9>"$HOKANN_CONTROL_FIFO"

  __hokann_prompt_command() {
    local current_history
    local last_command
    current_history=$(HISTTIMEFORMAT= builtin history 1)
    if [[ -n "$current_history" && "$current_history" != "$__hokann_last_history" ]]; then
      last_command=$current_history
      last_command=${last_command#"${last_command%%[![:space:]]*}"}
      last_command=${last_command#* }
      last_command=${last_command#"${last_command%%[![:space:]]*}"}
      printf 'HKP2\tSTART\t%s\0' "$last_command" >&9
      printf 'HKP2\tEND\t%d\t%s\0' "$__hokann_last_status" "$PWD" >&9
      __hokann_last_history=$current_history
    fi
    __hokann_prompt_id=$((__hokann_prompt_id + 1))
    printf 'HKP2\tPROMPT\t%d\t%s\t%s\0' "$__hokann_prompt_id" \
      "${HISTCONTROL:-}" "$PWD" >&9
  }

  __hokann_restore_status() {
    return "$__hokann_last_status"
  }

  __hokann_prompt_marker() {
    printf '\033]6973;hokann;1;%s;prompt;%d;%s\033\\' \
      "$HOKANN_SESSION_TOKEN" "$__hokann_prompt_id" "$HOKANN_PROMPT_CRC"
  }

  __hokann_apply() {
    local edit_payload next_cursor next_buffer
    edit_payload="$("${HOKANN_BIN:-hokann}" ipc take --session "$HOKANN_SESSION_TOKEN")" \
      || return 0
    next_cursor=${edit_payload%%$'\t'*}
    next_buffer=${edit_payload#*$'\t'}
    [[ "$next_cursor" =~ ^[0-9]+$ ]] || return 0
    READLINE_LINE="$next_buffer"
    READLINE_POINT=$next_cursor
  }

  bind -x '"\C-x\C-]":__hokann_apply'
  __hokann_original_prompt_command=${PROMPT_COMMAND:-}
  PROMPT_COMMAND='__hokann_last_status=$?; __hokann_restore_status;'
  if [[ -n $__hokann_original_prompt_command ]]; then
    PROMPT_COMMAND+=" $__hokann_original_prompt_command;"
  fi
  PROMPT_COMMAND+=' __hokann_prompt_command'
  PS1="${PS1}"'\[$(__hokann_prompt_marker)\]'
fi
"#
    .to_owned()
}

fn fish_script() -> String {
    r#"# hokann shell integration protocol 2
if test -n "$HOKANN_ACTIVE"; and test -n "$HOKANN_CONTROL_FIFO"; \
    and not set -q __HOKANN_FISH_LOADED; \
    and begin; test -z "$HOKANN_HOOK_OWNER_PID"; \
      or test "$HOKANN_HOOK_OWNER_PID" = "$fish_pid"; end
  set -gx HOKANN_HOOK_OWNER_PID $fish_pid
  set -g __HOKANN_FISH_LOADED 1
  set -g __hokann_prompt_id 0

  function __hokann_emit_prompt
    printf 'HKP2\tPROMPT\t%d\t%s\0' $__hokann_prompt_id "$PWD" >$HOKANN_CONTROL_FIFO
  end

  function __hokann_preexec --on-event fish_preexec
    printf 'HKP2\tSTART\t%s\0' "$argv[1]" >$HOKANN_CONTROL_FIFO
  end

  function __hokann_postexec --on-event fish_postexec
    set -l command_status $status
    printf 'HKP2\tEND\t%d\t%s\0' $command_status "$PWD" \
      >$HOKANN_CONTROL_FIFO
  end

  function __hokann_apply
    set -l edit_payload (command "$HOKANN_BIN" ipc take --session "$HOKANN_SESSION_TOKEN")
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

  functions -c fish_prompt __hokann_original_fish_prompt
  function fish_prompt
    set -g __hokann_prompt_id (math $__hokann_prompt_id + 1)
    __hokann_emit_prompt
    __hokann_original_fish_prompt
    printf '\033]6973;hokann;1;%s;prompt;%d;%s\033\\' \
      "$HOKANN_SESSION_TOKEN" $__hokann_prompt_id "$HOKANN_PROMPT_CRC"
  end

  bind \e\[99\~ __hokann_apply
  bind -M insert \e\[99\~ __hokann_apply
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
            assert!(script.contains("HOKANN_ACTIVE"));
            assert!(script.contains("ipc take"));
            assert!(script.contains("HKP2"));
            assert!(script.contains("6973;hokann;1"));
        }
        assert!(init_script(ShellKind::Zsh).contains("BUFFER="));
        assert!(init_script(ShellKind::Zsh).contains("__hokann_refresh_prompt"));
        assert!(init_script(ShellKind::Bash).contains("READLINE_LINE="));
        assert!(init_script(ShellKind::Fish).contains("commandline --replace"));
        for shell in [ShellKind::Zsh, ShellKind::Bash, ShellKind::Fish] {
            assert!(init_script(shell).contains("HOKANN_HOOK_OWNER_PID"));
        }
    }
}
