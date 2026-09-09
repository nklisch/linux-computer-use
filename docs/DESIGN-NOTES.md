# Design notes: field lessons from Linux desktop automation

These notes distill real incidents and qualifications from driving agents
against native Linux applications. Each entry states the symptom, what the
evidence showed, and the rule the codebase now follows. They are observations
from specific stacks (Fedora/Nobara, KDE Plasma 6, NVIDIA RTX 4070,
WebKitGTK 2.52, Godot 4.7, GTK 3.24), not universal laws — treat them as
hypotheses with unusually good provenance.

## Delivery, freshness, and application success are three different facts

**Symptom.** A `click` returned `input_status: sent`, a fresh capture, and a
newer frame sequence — and the application had not done the thing yet. In one
recorded case an agent sent a second click against that intermediate frame and
carved a second voxel cell it did not want (then had to right-click to undo).

**Evidence.** Immediate post-action frames from React apps and Godot UIs
routinely show hover/focus state before the action completes; a subsequent
observation — with no additional input — shows the completed state.

**Rule.** One gesture, then observe application state, then decide. A settle
delay improves legibility but is not a completion assertion. Nothing here ever
replays uncertain input automatically: repeating a click or paste can cause a
*second* effect. This is why every action result carries `outcome_verified:
false` forever — it is not a TODO.

## Black captures with healthy metadata are a real failure mode

**Symptom.** Observations returned black pixels with a visible cursor while
capture sequences advanced and freshness was met; the game behind them was
rendering fine (an independent game-side capture showed the scene).

**Evidence.** Coordinated re-checks with an unlocked, awake session produced
normal frames; a separate compositor screenshot matched. Screen blanking or
sleep during the original observations is plausible but unconfirmed.

**Rule.** Fresh metadata is evidence about the capture pipeline, not about
scene content. The diagnosis protocol: compare an independent capture of the
same output while it is visibly awake, before touching input. Never send input
against a screen you cannot see.

## Keyboard layout correctness means asking the compositor, not a table

**Symptom (avoided).** Most automation stacks map named keys to US positions,
so `SHIFT+TAB` on a German or Dvorak layout produces the wrong physical keys,
and older KWin versions lose modifiers on synthesized chords.

**Design.** For each gesture, LCU opens a short-lived Wayland connection,
reads the compositor's *actual* XKB keymap and active layout, resolves the
named chord to physical events, and sends those. If the map cannot be read,
the named gesture sends nothing and offers explicit physical evdev keycodes
as the alternative. No installed-version guessing, no US-position table.

**Adjacent quirk.** WebKitGTK reports native Shift+Tab to the DOM as
`key:"Unidentified", code:"Tab", shiftKey:true` (its logical-key mapping
omits `ISO_Left_Tab`); focus navigation still works. A consumer focus trap
can recognize that narrow combination. LCU preserves the physical chord
rather than synthesizing browser events.

## GTK3/WebKit on NVIDIA: a per-application graphics escape hatch

**Symptom.** A GTK3 window containing a WebKit2 WebView (including Tauri 2
shells) exits with `GDK Error 71` in a virtual KWin session: the compositor
rejects the commit — `explicit sync is used, but no acquire point is set` —
when the DMA-BUF surface lacks its acquire point.

**What was tried, on this stack.**

- `env: {"GDK_GL": "always"}` — **qualified**: visibly rendered with a
  correctly synchronized DMA-BUF commit (acquire point precedes attach), for
  both the fixture and real Tauri shells.
- `__NV_DISABLE_EXPLICIT_SYNC=1` — avoided Error 71 but left the WebView
  blank; **not** a fix.
- `WEBKIT_DISABLE_DMABUF_RENDERER=1` — renders, but disables the DMA-BUF
  renderer entirely; broader hammer, kept as a deliberate recovery option.

**Rule.** Graphics workarounds are launch-local environment settings on the
affected application — never global defaults, never claims that the upstream
default path is fixed. One NVIDIA stack is not all NVIDIA stacks.

## Godot: explicit display driver beats version sniffing

**Symptom.** Versioned Godot executables (`Godot_v4.7.1_stable_linux.x86_64`)
fell through to generic launch routing, logged `X11 Display is not available`,
then fell back to Wayland and worked — wasting seconds per launch. One editor
configuration went further: black pixels plus `Could not create render target`
on OpenGL Compatibility.

**Resolution.** Explicit `--display-driver wayland` removes the failed X11
detour (recognized routing handles plain `godot`/`godot4` names). The editor
case cleared with `--rendering-method mobile` (Vulkan); renderer and driver
changed together, so attribution stays uncertain.

**Rule.** Prefer explicit, user-visible arguments over inferring intent from
executable filenames. A process receipt is not a visible window; observe
before believing a launch.

## An application can succeed and still not exit

**Symptom.** A Godot headless-style profiling script printed its success
marker and full summary, then hung in `futex_wait` past a 300-second
deadline; the same script exited normally on retry.

**Rule.** Do not treat an application-level success marker as proof that the
process lifecycle finished. Keep bounded deadlines, keep the retry explicit,
and never widen a timeout into "wait forever" because the marker printed.

## Pointer capture is application state, not input state

**Symptom.** After a claim handoff, `move_relative` reported sent + fresh
frame, but the game camera did not move. A click inside the window, then
relative motion, moved the camera as intended.

**Rule.** Relative look depends on the application having pointer capture.
`expected_focus_node_id` checks application-reported focus before a gesture;
for games, observe → click to (re)capture → then move. The uncertain action
was never replayed blindly.

## Capture pacing: copy early, coerce nobody

The capture path copies pixels out of PipeWire's finite buffer pool
immediately, coalesces surplus buffers, and converts at roughly 15 fps. The
copy is the point: downstream pacing must never hold buffers the compositor
needs for its next repaint. Timestamps describe when *this process* received a
frame; idle desktops legitimately reuse a cached frame with reported sequence
and age. Frames are never manufactured for an idle source.

## Claims are coordination, not leases

A claim says who may send input *through this tool*. It cannot fence humans,
unrelated automation, or another compositor participant. Force-claim is an
explicit preemption that waits for ordered input cleanup before granting the
successor; a failed cleanup stays retryable and grants nothing — uncertain
held input must never be inherited. Claim age never steals. Disconnect
releases claims, not desktops; workers survive daemon restarts; nothing
expires silently.

## Clipboard: pasting is a visible, stateful act

`type` pastes through the desktop's clipboard and replaces its contents. On an
owned desktop that clipboard is private; on `main` it is yours, and whatever
an agent types becomes the next paste for whoever reads it. This is one
reason owned desktops exist: input isolation includes clipboard selection.

## Publish examples when schemas are deep

Agents repeatedly guessed gesture shapes (`{"type":"key"}` instead of
`keypress`) when an MCP gateway rendered a nested enum as `action *required*`.
The schema was correct; the disclosure was shallow. Tools with tagged-union
inputs should publish worked examples — the README carries them for exactly
this reason.
