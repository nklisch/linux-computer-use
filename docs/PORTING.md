# Porting notes: where this assumes KDE

LCU is deliberately **not** a compositor-provider framework. It is a working,
qualified implementation for one desktop: KDE Plasma 6 on Wayland. This document
maps exactly where that assumption lives, what is shared freedesktop
infrastructure, and what qualifying another Wayland desktop would actually
touch. If you want LCU-style behavior on GNOME, Sway or something else, this is
the work list — and the honest boundary between "expected to work" and
"qualified".

## Layer map

| Concern | Mechanism | Portability |
|---|---|---|
| Input + capture authorization | `xdg-desktop-portal` RemoteDesktop + ScreenCast + Clipboard | Standard portal APIs; behavior varies by backend implementation |
| Owned desktop compositor | `kwin_wayland --virtual` subprocess | **KWin-specific** (`src/desktop/session.rs`) |
| Unattended owned-desktop access | Pre-written `kde-authorized` / `remote-desktop` permission in a private permission store | **KDE portal backend-specific** |
| Portal backend in owned sessions | `xdg-desktop-portal-kde` activated on a private session bus | Backend-specific startup |
| Image capture | PipeWire stream → GStreamer (`pipewiresrc` → `appsink`) | Standard; stream parameters may vary by backend |
| Keyboard chords | Compositor's live XKB keymap read from a fresh Wayland connection, resolved with `xkbcommon` | Standard Wayland/XKB; no port work expected |
| Physical input fallback | Linux evdev keycodes through the portal | Standard |
| Scroll direction | Portal's vertical axis sign normalized to positive-down | Backend-specific sign convention; currently normalized for KDE |
| Accessibility | AT-SPI over D-Bus (`src/accessibility.rs`) | Standard; application-dependent |
| Process supervision / cleanup | Linux subreaper, `/proc`, exec gate, lifecycle locks | Standard Linux |
| Frontends: MCP, CLI, daemon RPC, monitor | Rust code, Unix sockets | Compositor-agnostic |

## The two hard KWin dependencies

### 1. Headless virtual sessions

An owned desktop is built by `bootstrap()` in `src/desktop/session.rs`:

1. A private `dbus-daemon` session bus under the desktop's runtime directory.
2. `kwin_wayland --virtual --width W --height H --output-count 1 --socket lcu-wayland --no-lockscreen` — a real KWin instance with a virtual output, in its own private `XDG_RUNTIME_DIR`, `HOME`, config/data/cache/state paths, and host `PIPEWIRE_REMOTE` wiring.
3. `/usr/libexec/xdg-permission-store` (note: a distro-specific path — Fedora-style; patch for elsewhere) on the private bus.
4. A pre-written permission (`kde-authorized` table, `remote-desktop` entry) so the portal never shows an interactive prompt inside a desktop nobody can see.
5. `StartServiceByName("org.freedesktop.portal.Desktop")` and letting the private bus coalesce backend activation.

Porting to another desktop environment means answering *"what is your
headless compositor + private portal story?"* — this function is the whole
seam, and everything else (controller, capture, claims, cleanup) sits on top
of the socket and bus it produces. GNOME has `gnome-shell --headless`, but the
permission pre-authorization step is KDE-shaped and would need an equivalent
unattended-authorization mechanism, or an explicitly different policy
(e.g. require the user to authorize each owned desktop).

### 2. Portal backend behavior

The `xdg-desktop-portal` API is standard, but implementations differ in ways
that matter:

- **Vertical scroll sign** is reversed by the KDE backend relative to the
  RemoteDesktop spec's natural reading; `src/portal.rs` normalizes to
  positive-down. Probe this before trusting scroll on a new backend.
- **Restore tokens** (saved permission restoration) are backend-dependent;
  the code stores whatever the backend returns and reuses it.
- **Session closure semantics** drive the cleanup contract ("failed portal
  closure never grants a successor over uncertain held input").

## What a qualification actually requires

Advertised headless support is insufficient; the reference qualifications for
this machine exercised, on a disposable owned desktop:

- capture with correct geometry and per-stream diagnostics,
- absolute clicking, Unicode paste, scrolling (correct sign), dragging, holds,
- keymap-correct chords on a non-US layout,
- accessibility inspect + focus + `expected_focus_node_id` refusal,
- held-input release on cancellation and emergency stop,
- application routing for Chromium and Godot,
- worker survival across daemon restart, and explicit destroy cleanup.

`scripts/desktop_fixture.py` (GTK window with probes), `scripts/webkit_fixture.py`
(WebKitGTK with key/scroll probes) and `scripts/live_session.py` (persistent
CLI bridge) are the fixtures. They are disposable by construction — copy the
pattern rather than the machine-specific results.

## X11

Out of scope. Input injection would go through XTEST instead of a portal,
capture through a different path entirely, and the owned-desktop construction
would need a different isolation story. Treat a port as a new project that
reuses the controller/claims/cleanup design, not this codebase.

## Why not an abstraction layer

A provider interface with one implementation is untested generality: it hides
which backend actually verified what, and every backend difference above
(scroll sign, restore tokens, closure semantics) leaks through the abstraction
anyway. When a second desktop is genuinely qualified, extract the seam from
two real implementations — `session.rs` and `portal.rs` are small enough that
this stays cheap.
