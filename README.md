# Linux Computer Use

Give local agents a reliable way to observe and operate graphical Linux applications — through **Model Context Protocol (MCP)** or a direct CLI — without taking over your desktop by accident.

Each agent can work in its own **owned desktop**: an independent headless KDE/Wayland session with private pointer, keyboard focus, clipboard and application settings, created and destroyed on demand. Applications launch into a selected desktop; capture uses PipeWire; input goes through the desktop portal. One local daemon serves every frontend.

**This is interaction isolation, not a security sandbox.** Owned desktops share your filesystem and user privileges. A claim coordinates participating tools; it cannot fence out humans or unrelated automation. And successful input delivery never proves the application did what you meant — the tool reports delivery and freshness as separate facts, and never replays uncertain input for you.

## Who is this for

- Anyone building or running **computer-use agents on Linux** who wants grounded screenshots, precise input, and honest feedback instead of a vague "success".
- Anyone who wants a **working reference architecture** for Linux desktop automation: portals, PipeWire capture, keymap-correct keyboard synthesis, process supervision, claim-based coordination, cancellation-safe cleanup. The code is deliberately readable about its boundaries — see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/PORTING.md](docs/PORTING.md) and [docs/DESIGN-NOTES.md](docs/DESIGN-NOTES.md) for the hard-won details.

Supported configuration, stated honestly:

| Layer | Status |
|---|---|
| KDE Plasma 6, Wayland, `xdg-desktop-portal-kde` | Qualified (developed here; NVIDIA GPU stack, Fedora) |
| Other Wayland compositors (GNOME, Sway, …) | Not qualified — read [docs/PORTING.md](docs/PORTING.md) first |
| X11 | Not supported |

The capture/input plumbing below it (portals, PipeWire, xkbcommon, AT-SPI) is standard freedesktop infrastructure; the compositor-specific parts are isolated and documented.

## Install

One-line install of the latest release binary into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/nklisch/linux-computer-use/main/scripts/install-release.sh | sh
lcu doctor
```

Or build from source with Rust 1.92+ and the system libraries below:

```sh
git clone https://github.com/nklisch/linux-computer-use
cd linux-computer-use
sh scripts/install.sh        # cargo install --path, honors LCU_INSTALL_ROOT
lcu doctor
```

Build dependencies (for the `lcu` binary itself; the optional monitor needs more, below):

| Distribution | Packages |
|---|---|
| Fedora | `libxkbcommon-devel gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-pipewire` |
| Ubuntu/Debian | `libxkbcommon-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-pipewire` |
| Arch | `libxkbcommon gstreamer gst-plugins-base gst-plugin-pipewire` |

You also need a running `xdg-desktop-portal` with the KDE backend inside your Wayland session. `lcu doctor` reports portal versions and capture plugins without requesting desktop access.

### Connect an MCP client

Use the repository's [`.mcp.json`](.mcp.json) with clients that support that format, or register a stdio server with command `lcu` and arguments `["mcp"]`. Use the full binary path if your client doesn't inherit your shell's PATH. Merely connecting requests nothing; desktop access is a separate, user-approved step.

## The model in one minute

- A **desktop** is a graphical session with its own screens, pointer, keyboard focus and clipboard. `main` is your existing desktop; every other desktop is one this tool created and owns.
- A **claim** says who may send input through this tool. Claims belong to live connections: disconnect and the claim releases while the desktop and its applications stay. `force: true` explicitly preempts; claim age never does.
- **Observation** returns an image plus capture evidence (`frame_id`, sequence, ages). Coordinates for actions refer to the returned image, and only the current claimant's latest observation is actionable.
- **Actions** report `input_status` (`not_sent` / `sent` / `possibly_partial`) and always `outcome_verified: false`. Delivery, freshness and application success are distinct facts; verifying the application result is the caller's job.

```json
{
  "desktop_id": "COPY_FROM_LIST",
  "frame_id": "COPY_FROM_LATEST_OBSERVATION",
  "action": { "type": "click", "x": 420, "y": 280 }
}
```

Supported gestures: `move`, `move_relative`, `click`, `scroll`, `keypress`, `type`, `drag`, `hold`. MCP publishes the full input schema. Scroll deltas are positive right/down. `type` pastes arbitrary Unicode through the clipboard (terminals may need `paste_keys: ["CTRL","SHIFT","V"]`). Named key chords are resolved through the running compositor's actual keymap — preserving German/French/Dvorak layouts — with explicit physical evdev `keycodes` as the layout-independent alternative.

## Agent workflow

1. `computer_desktop` `operation:"list"` / `"create"` — choose or create a desktop. `main` is the shared desktop; everything else is agent-owned.
2. `operation:"claim", desktop_id: ID` — claims belong to this live connection. Owned control opens unattended; `main` needs `computer_start` plus your portal authorization (an agent must never approve its own access prompt).
3. `operation:"launch"` with `argv` (argument vector, not a shell string) and absolute `cwd` — start applications in the desktop.
4. `computer_observe` — returns the image and a `frame_id`.
5. `computer_act` with that desktop and frame — then inspect the result before more input. `observe:false` suppresses the post-action image; you must observe again before the next action.
6. `release` keeps applications; `destroy` explicitly reclaims an owned desktop and its private settings.

Notes that matter in practice:

- **Freshness**: pass `after_sequence` + `timeout_ms` (0–5000) to `computer_observe` to wait for a frame newer than your last one. Timeout returns cached pixels with `freshness_met:false` — a quiet desktop legitimately returns cached frames.
- **Intermediate frames**: the immediate post-action image can show hover/focus before the application finishes. One gesture, then observe, then decide — replaying against an intermediate frame can undo your own action (see [docs/DESIGN-NOTES.md](docs/DESIGN-NOTES.md)).
- **Inspect/focus**: `computer_inspect` exposes AT-SPI accessibility metadata; `computer_focus` requests focus on an inspected node, and `expected_focus_node_id` on `computer_act` refuses to send if that node lost focus. Application-reported evidence, not a desktop lock; screenshots remain the fallback.
- **Recovery**: after a capture failure or layout change, `computer_start {"restart":true}` recreates the session with the saved permission. Nothing is ever automatically replayed.
- **Emergency stop**: `lcu stop` reaches every worker independent of the daemon and MCP, cancelling input and releasing held keys without destroying applications. `lcu stop --desktop ID` targets one.

## CLI

```sh
lcu daemon start
lcu desktop create --width 1280 --height 720 --name "Browser checks"
lcu desktop list
lcu desktop launch DESKTOP_ID --cwd /path/to/project -- godot --editor --path /path/to/project
lcu screenshot --desktop DESKTOP_ID --output /tmp/desktop.png
lcu session --desktop DESKTOP_ID --claim
lcu desktop destroy DESKTOP_ID
```

The CLI never goes through MCP. `session` is a persistent JSON-lines control channel that holds a claim across commands — see `lcu session --help`. One-shot `launch` temporarily claims and explicitly releases; it fails if another connection owns the desktop.

## Desktop monitor (optional Qt companion)

`lcu monitor` is a native Qt Widgets window for watching owned desktops: a refreshable grid of previews, click-to-enlarge read-only viewing, and explicitly confirmed destroy. Viewing never claims or forwards input.

```sh
# Requires Qt 6 base development libraries, CMake, a C++17 compiler.
sh scripts/install-monitor.sh
```

The monitor is connect-only: it never creates desktops, authorizes capture, or forwards input.

## Application routing

Owned sessions use private `HOME`/XDG settings and route to the host PipeWire server. Chromium-family launchers get a desktop-private profile and Wayland; `godot`, `godot4` and official versioned Godot 4 names get Wayland routing. Arbitrary commands run with the private environment — `routing: private_environment_only; application routing unverified` — so single-instance apps may or may not land in the right desktop. A process receipt is not proof of a visible window.

For GTK3/WebKit applications on some NVIDIA stacks, launch-local `env: {"GDK_GL": "always"}` renders correctly where the default path hits a Wayland protocol error — see [docs/DESIGN-NOTES.md](docs/DESIGN-NOTES.md) before applying it.

## Stop immediately

```sh
lcu stop
```

Reaches workers independently of the daemon and MCP. Cancels control and releases held input without destroying applications. Never use global stop to clean up one agent's session — use `lcu stop --desktop ID` or release the claim.

## Documentation

- [docs/VISION.md](docs/VISION.md) — what this is trying to be, and its boundaries.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — daemon/workers/controllers, ownership, lifecycle and cleanup design.
- [docs/PRINCIPLES.md](docs/PRINCIPLES.md) — the engineering rules that shaped every decision.
- [docs/PORTING.md](docs/PORTING.md) — exactly where the implementation assumes KDE/KWin, and what qualifying another Wayland desktop would touch.
- [docs/DESIGN-NOTES.md](docs/DESIGN-NOTES.md) — distilled field notes: keyboard layout correctness, WebKit/GTK graphics quirks, Godot routing, capture pacing, and why the feedback model looks like it does.

## Building and testing

```sh
cargo fmt --all --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
sh scripts/test-monitor.sh    # optional monitor: offscreen Qt tests against a typed Rust fixture
```

Automated tests cover image geometry, capture layout and freshness, input cleanup, keyboard resolution, and real CLI/MCP process transports. **They never request desktop access** — CI runs them on a stock runner. Live acceptance uses the disposable fixtures in `scripts/` (`desktop_fixture.py`, `webkit_fixture.py`, `live_session.py`) against explicitly owned throwaway desktops.

## Scope and limitations

- Not a sandbox: owned desktops share your filesystem and privileges; isolation covers input focus, clipboard and application settings.
- Claims coordinate participants of this tool only; humans and other automation can always interfere (especially on `main`).
- Input delivery is never automatically replayed, and `outcome_verified` is always the caller's to establish.
- Qualified on KDE Plasma 6 / Wayland / KWin virtual sessions. GNOME, Sway and other compositors need qualification — [docs/PORTING.md](docs/PORTING.md) maps the work.
- One machine's NVIDIA stack is not every NVIDIA stack; graphics quirks are per-application launch settings here, never global defaults.

## License

MIT — see [LICENSE](LICENSE).
