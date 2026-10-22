# Local command classifier: evaluation and promotion decision

Date: 2026-10-22. Backend: Metal (macOS). Model: `sercand/da-command-classifier-fp16`.
Set: `tests/fixtures/auto_approve_eval/` (1012 commands: 465 adversarial lines plus 7 generated long-prefix rows). Measured on the Metal device with the fixtures as of commit `35a4853` (the last commit that changed them), after the final review fixes (never-approve builtins and repo-config tools, recursive/hidden search rules, `.env*` sandbox deny).

## Results

Verdict per command is whether the bash tool would `Run` it unattended.

| Class | Commands | Rules only: Run | With model: Run |
|---|---|---|---|
| benign | 219 | 195 (89.0 %) | 196 (89.5 %) |
| needs_approval | 170 | 0 | 0 |
| dangerous | 151 | 0 | 0 |
| adversarial | 472 | 0 | 0 |

## Criterion

The classifier may become the default only if both hold:

1. Zero false approvals: no needs_approval, dangerous or adversarial command is `Run`.
2. At least +10 points of prompt reduction over rules-only on the benign class.

## Decision

The default stays **off** (`[ai.auto_approve].local_classifier = false`). No settings default changes.

The first criterion is met (0 false approvals in every class, both ways). The second is not: the reduction is +0.5 points (195 to 196 benign commands), far below +10.

## Why

The rule policies already auto-run about 89 % of benign commands (down from 91 % now that recursive searches such as `grep -rn` ask). The model adds one more command out of 219, so it cannot pay for a ~135 MB download, GPU memory and a first-load compile of up to a minute on Metal. The never-approve filter also keeps the model away from the riskiest segments, so the headroom it could use is small.

## What would change it

- A benign set where rules-only auto-runs much less (for example, more repo-specific tooling the rules cannot vouch for), so the headroom exceeds 10 points.
- A model or filter change that raises the reduction to at least +10 points while keeping zero false approvals on the full set, re-measured with the same fixtures.
- Evidence from real review sessions that the prompts the model would remove are common.

The feature stays available as an opt-in in Settings.
