---
title: Your first shell session
description: Learn the small set of keys and commands that make Hokan useful in a real terminal.
order: 2
---

After installation, Hokan starts the underlying shell you already use. Type naturally and pause when you want context. Hokan never replaces the prompt and `Tab` never executes a suggestion.

## Keys

| Key | Action |
| --- | --- |
| `Up` / `Down` | Move the selection |
| `PageUp` / `PageDown` | Move between pages |
| `Tab` | Insert the selected or highest-ranked candidate |
| `Enter` | Run typed input, or an explicitly selected candidate |
| `Esc` | Close the overlay or cancel an AI request |
| `Ctrl-R` | Toggle the history-focused view |
| `Shift-Tab` | Show or hide the completion list |

## Leave Hokan for this session

When Hokan is active, run:

```bash
hokan-leave
```

This returns to the underlying shell without changing the rest of your installation. Plain `exit` and `Ctrl-D` keep their normal behavior.

## Disable automatic startup once

```bash
HOKAN_AUTO_START=0 zsh -l
```
