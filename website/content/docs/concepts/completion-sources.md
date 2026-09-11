---
title: Completion sources
description: Understand the context Hokan combines and how suggestions are ranked.
order: 3
keywords:
  - completion sources
  - ranking
  - shell history
  - Git
---

Hokan merges independent providers into one ranked list. A suggestion can come from the current shell, the project around it, or an explicit AI action.

| Source | Examples |
| --- | --- |
| Shell history | zsh, bash, and fish history with fuzzy search and failure penalties |
| Command specs | Built-in recipes, flags, subcommands, argument slots, and risk levels |
| Help pages | Read-only extraction from man pages for commands without a built-in spec |
| Filesystem | Files, directories, executable scripts, quoting, spaces, and Unicode paths |
| Project metadata | npm, pnpm, yarn, bun, Deno, Cargo, Make, just, and workspace members |
| Git state | Contextual `init`, `clone`, `status`, `add`, `commit`, `push`, and `pull` |
| Shell definitions | Aliases, functions, abbreviations, and inferred argument slots |
| System state | Running processes, PIDs, and network interfaces |
| AI action | An explicit OpenAI-compatible request selected by you |

Suggestions are inserted into the command line for review. The ranking layer can prefer a recent successful command, a project script, or an argument that fits the current slot without taking execution away from you.
