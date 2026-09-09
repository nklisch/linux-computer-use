# Agent Instructions

Reference implementation for agent-driven Linux desktop control (KDE/Wayland).
Read `docs/PRINCIPLES.md` before changing behavior — the honest-feedback
semantics (`input_status` vs `outcome_verified`, never auto-replay) and
cancellation-safe cleanup are the point of this codebase, not incidental.

## Verification

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
sh scripts/test-monitor.sh   # optional monitor; needs Qt 6 dev + CMake
```

Rules that are not negotiable:

- Automated tests must never request desktop permission or send desktop input.
  CI has no display; keep it that way.
- Live checks use the disposable fixtures in `scripts/` against explicitly
  owned throwaway desktops, never the main desktop.
- Never call global `lcu stop` to clean up one session; use `lcu stop --desktop`
  or release the claim.
- Qualification is native evidence: a mocked event trace is not proof of
  application behavior, and advertised headless support is not qualification.
  See `docs/PORTING.md` before touching compositor-specific seams.

Keep durable rationale in `docs/`; the README owns the quickstart. Code owns
request/response structure, documents own semantics — do not maintain parallel
hand-written schemas.
