# Linux Computer Use

Linux Computer Use gives local agents a reliable way to observe and operate graphical applications without taking over the user's work by accident. It targets KDE Plasma on Wayland; the seams and assumptions are documented in [PORTING.md](PORTING.md).

A **desktop** is a graphical session with its own screens, pointer, keyboard focus and clipboard. The **main desktop** is the user's existing session. An **owned desktop** is a separate session created and managed by LCU for agent work.

## Shared and isolated work

The main desktop remains useful for applications the user already has open. Access requires desktop permission, and an agent must account for other participants changing focus or moving windows. A successful input call does not prove that its intended application received the input.

Owned desktops give agents independent graphical sessions. Agents launch applications into a selected desktop and can work concurrently without sharing pointer position, keyboard focus or clipboard selection. Desktop creation should be headless and unattended after the user has authorized this capability; it must not weaken the main desktop's permission policy.

Agent disconnection releases its input claim but leaves the desktop and applications available for reconnection until explicitly closed. A native desktop monitor gives the user a preview overview and enlarged read-only viewing without claiming control. Closing the monitor retains applications; destroying a desktop requires explicit confirmation. Human takeover is outside this viewing capability.

## Boundaries

Owned desktops use private application profiles while sharing the user's project filesystem. They are interaction isolation, not security sandboxes: applications still run with the user's filesystem privileges. Separate checkouts or worktrees remain necessary when concurrent agents would otherwise modify the same files.

A **claim** coordinates who may send input through LCU. A claim on the main desktop cannot prevent human input or unrelated automation. Owned desktops must never fall back to the main desktop when their session fails.

LCU prioritizes trustworthy feedback, recoverable failures and bounded resource ownership. It does not infer task completion from event delivery, replay uncertain actions automatically, or hide lost guarantees behind a successful tool result.
