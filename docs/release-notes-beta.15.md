# v0.1.0-beta.15

This beta fixes history navigation that stopped after the first Up press and
stale inline suggestions left behind after accepting a completion.

- Arrow-key history shows the newest entry at the bottom. Up moves toward
  older commands and Down returns toward newer commands; arrow and page keys
  wrap at both ends, matching the other completion lists.
- Repeated history keys are retained while the first results are loading,
  including presses that wrap through the list.
- Arrow-key history uses `completion.max_candidates` (1000 by default) instead
  of a fixed 50-entry cap. Current-directory and input-prefix filtering remain
  in effect, and `ui.max_rows` still controls the visible height.
- Zsh clears the old inline suggestion when filling or executing a selected
  command, so its gray suffix no longer survives or moves forward as you type.

Real-terminal certification remains subject to the
[compatibility matrix](https://github.com/backrunner/hokan/blob/v0.1.0-beta.15/docs/compatibility.md);
this beta does not expand those certification claims.

Existing beta users can update with:

```sh
hokan upgrade --channel beta --yes
```

Restart Hokan after upgrading to use the new binary.
