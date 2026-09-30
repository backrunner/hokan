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
| Help pages | Documented flags and subcommands from help and man pages, supplementing built-in recipes |
| Filesystem | Files, directories, executable scripts, quoting, spaces, and Unicode paths |
| Project metadata | npm, pnpm, yarn, bun, Deno, Cargo, Make, just, and workspace members |
| Git state | Contextual `init`, `clone`, `status`, `add`, `commit`, `push`, and `pull` |
| Shell definitions | Aliases, functions, abbreviations, and inferred argument slots |
| System state | Running processes, PIDs, and network interfaces |
| AI action | An explicit OpenAI-compatible request selected by you |

Suggestions are inserted into the command line for review. The ranking layer can prefer a recent successful command, a project script, or an argument that fits the current slot without taking execution away from you.

The list keeps all matching candidates by default and shows them in pages. Arrow keys and PageUp/PageDown wrap at both ends. An exact command or subcommand does not hide longer names sharing its prefix. Slower providers continue adding results after the first batch; an explicit `completion.max_candidates` limit can still cap the final list.

Help-based discovery follows the installed CLI's command tables and nested scopes. It accepts commands with no description, single-space columns, long names, grouped sections, and colored or hyperlinked help text. Cargo's installed command list supplements its abbreviated root help, so installed extensions can also appear. Python module recommendations keep all discovered matches rather than stopping at 500.

Help probes run in the background with bounded time and output. A CLI that does not document its commands in a recognized format may still need a command specification; an explicitly abbreviated help table is not treated as an exhaustive list for validating history.
