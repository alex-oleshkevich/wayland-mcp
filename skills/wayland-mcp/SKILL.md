---
name: wayland-mcp
description: Use when seeing, operating, testing or debugging a Linux Wayland desktop from the shell with wayland-mcp - screenshots, OCR, pointer/keyboard/touch input, accessibility, clipboard and visual verification. Trigger when the user asks to look at, click, type into or screenshot their desktop or an app through desktop automation; use gtk-mcp instead for in-process GTK widget inspection.
---

# wayland-mcp

Drive a Linux Wayland desktop through the `wayland-mcp` CLI.

## First step

Run:

```sh
wayland-mcp prime
```

It prints the full usage guide and tool index. Follow it.

If `wayland-mcp` is not installed:

```sh
curl -fsSL https://raw.githubusercontent.com/alex-oleshkevich/wayland-mcp/main/install.sh | sh
```

Run inside the user's Wayland session. `wayland-mcp doctor` reports prerequisites
and capabilities without prompting or changing settings. GNOME/KDE host control
normally needs `--portal` and the user's consent.

## Rules

- Look before acting: screenshot or get_app_state, locate the target, input, then verify.
- Check arguments with `wayland-mcp tools NAME` instead of guessing.
- Each `call` is a fresh server. Shot ids, sandboxes, held input, clipboard ownership,
  owned apps, portal sessions, active traces and recording handles do not survive it.
  Follow `prime` for logical-coordinate conversion, batch limits and persistent MCP workflows.
- Save images with `-o FILE.png` and view them to verify visual changes.
- Host input affects the user's desktop; respect portal denial and confirm destructive actions.
