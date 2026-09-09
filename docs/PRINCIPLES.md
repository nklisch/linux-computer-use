# Engineering Principles

## Report what happened, not what was intended

Input delivery, capture freshness and application success are distinct facts. Keep them separate in results and tests. Never automatically replay an uncertain action: repeating a click, paste or movement can cause another effect.

## Own only what this tool creates

Release held input on completion, cancellation and failure. Close task-owned sessions and processes without stopping other agents or the user's applications. Main-desktop access does not make LCU the owner of the compositor. Agent-created desktops must fail within their own boundary rather than redirect to the main session.

## Fit reliability to the product

Protect credentials, private data and irreversible actions. Refuse a requested operation when continuing would misdirect input, mislead the caller or make recovery harder; otherwise degrade at the narrowest useful boundary and expose the lost guarantee. A missing optional capability must not disable unrelated desktop control. Name the concrete failure a guard prevents and provide an actionable alternative.

## Keep one authority for each contract

Code owns structural request and response definitions. Documentation owns meaning, guarantees, boundaries and rationale. Derive mechanical representations instead of maintaining parallel schemas or variant lists.

## Earn compatibility obligations

Preserve real external consumers and substantial user data. Do not add versioned schemas, dual-read paths or deprecation machinery solely because an internal interface already exists. Plan genuine data migrations explicitly; production data changes belong to the user.

## Leave the touched area simpler

Prefer clear, cohesive modules and fewer independent concepts. Remove obsolete branches, duplicated state and checks made unnecessary by the change while preserving meaningful guarantees and measured performance. Do not impose a generic ports-and-adapters framework on a small tool; introduce a boundary when concrete consumers or ownership needs justify it.

## Test behaviors that matter

Use stable interfaces and disposable fixtures to test meaningful outcomes, cleanup, transport contracts and real regressions. Native behavior needs native evidence; a mocked event trace is not proof of application success. Tests must earn their maintenance cost rather than chase every branch.

## Make isolation claims precise

A separate graphical session isolates input focus and clipboard selection. It does not isolate filesystem privileges. Cooperative claims coordinate LCU participants, not every possible actor on the machine. Document these limits where users encounter them.
