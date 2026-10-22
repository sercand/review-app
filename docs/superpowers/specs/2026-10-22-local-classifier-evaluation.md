# Local command classifier: evaluation and promotion decision

Date: 2026-10-22. Backend: Metal (macOS). Model: `sercand/da-command-classifier-fp16`.
Set: `tests/fixtures/auto_approve_eval/` (919 commands).

## Results

Verdict per command is whether the bash tool would `Run` it unattended.

| Class | Commands | Rules only: Run | With model: Run |
|---|---|---|---|
| benign | 219 | 199 (90.9 %) | 200 (91.3 %) |
| needs_approval | 163 | 0 | 0 |
| dangerous | 151 | 0 | 0 |
| adversarial | 390 | 0 | 0 |

## Criterion

The classifier may become the default only if both hold:

1. Zero false approvals: no needs_approval, dangerous or adversarial command is `Run`.
2. At least +10 points of prompt reduction over rules-only on the benign class.

## Decision

The default stays **off** (`[ai.auto_approve].local_classifier = false`). No settings default changes.

The first criterion is met (0 false approvals in every class, both ways). The second is not: the reduction is +0.4 points (199 to 200 benign commands), far below +10.

## Why

The rule policies already auto-run about 91 % of benign commands. The model adds one more command out of 219, so it cannot pay for a ~135 MB download, GPU memory and a first-load compile of up to a minute on Metal. The never-approve filter also keeps the model away from the riskiest segments, so the headroom it could use is small.

## What would change it

- A benign set where rules-only auto-runs much less (for example, more repo-specific tooling the rules cannot vouch for), so the headroom exceeds 10 points.
- A model or filter change that raises the reduction to at least +10 points while keeping zero false approvals on the full set, re-measured with the same fixtures.
- Evidence from real review sessions that the prompts the model would remove are common.

The feature stays available as an opt-in in Settings.
