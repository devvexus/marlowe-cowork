---
name: all-keys
description: Exercises every frontmatter key Claude Code documents for subagents (Marlowe-authored fixture).
tools: Read, Grep, Glob, Bash(git log:*), mcp__github
disallowedTools: Write, Edit
model: haiku
effort: high
permissionMode: plan
skills:
  - access
  - example-skill
mcpServers:
  github:
    command: gh-mcp
hooks:
  PreToolUse:
    - matcher: "Bash"
      hooks:
        - type: command
          command: "./check.sh"
maxTurns: 12
isolation: worktree
color: blue
initialPrompt: "/review"
memory: project
background: false
favouriteColour: green
---

You review one change at a time and never edit files.
