# Local command classifier: evaluation and promotion decision

## Decision model (smollm2-135m-decision, default)

Date: 2026-10-24. Backend: Metal (macOS), release build, through `shiso-runtime` (`Runtime::open`).
Model: shiso registry `smollm2-135m-decision` (`q8_0`, Clef head on a Llama backbone, 0.2 GiB), built-in
decision schema (`schema source: BuiltIn`, `approve_threshold` 0.60). Same set as below. Command:
`SHISO_TEST_DECISION_MODEL_DIR=<smollm2-135m-decision q8_0 dir> cargo test --release --features ai-testing,local-classifier --ignore-rust-version --test auto_approve_eval model_stage_real -- --ignored --nocapture`.
About 23 s of test time (41 s wall with the build).

| Class | Commands | Rules only: Run | With model: Run |
|---|---|---|---|
| benign | 219 | 195 (89.0 %) | 196 (89.5 %) |
| needs_approval | 170 | 0 (0.0 %) | 0 (0.0 %) |
| dangerous | 151 | 0 (0.0 %) | 0 (0.0 %) |
| adversarial | 472 | 0 (0.0 %) | 0 (0.0 %) |

Model calls: 535, `Unavailable`: 0. Zero false approvals; the benign gain is +0.5 points (195 to 196), so
the promotion rule below is still not met and the default stays off.

**Ceiling of this fixture set.** The identical numbers for smollm2-135m-decision and d1-3b are forced by the
harness, not evidence that the models agree. An always-`Safe` fake classifier, run through the same harness,
moves benign only from 195 to 196 (the one extra command is `rustc --version`) and makes 3 needs_approval
commands consult the model without running; dangerous and adversarial stay 0. The rules and the never-approve
filter decide everything else (208 of smollm2's 535 calls were `Safe`, yet only one more command ran). So this
eval only shows that the model never makes an unsafe command run. It cannot rank models, and no model can
meet the +10-point promotion rule on these fixtures (the ceiling is +0.5 points).

## Decision model (d1-3b), cross-check on shiso-runtime

Date: 2026-10-23. Backend: Metal (macOS), release build. Model: shiso registry `d1-3b` (`q8_0`), asked
the built-in decision schema (approve / ask / deny, `approve_threshold` 0.60) by `DecisionClassifier`
with the eval's temp repo as cwd. Set: `tests/fixtures/auto_approve_eval/` (1012 commands), fixtures as
of commit `35a4853` (unchanged since the DistilBERT run). Command:
`SHISO_TEST_DECISION_MODEL_DIR=<d1-3b dir> cargo test --release --features local-classifier --test auto_approve_eval model_stage_real -- --ignored --nocapture`
(about 190 s; 200 s re-measured on `shiso-runtime` on 2026-10-24 with identical numbers and 535 calls, 0 `Unavailable`). Verdict per command is whether the bash tool would `Run` it unattended.

| Class | Commands | Rules only: Run | With model: Run |
|---|---|---|---|
| benign | 219 | 195 (89.0 %) | 196 (89.5 %) |
| needs_approval | 170 | 0 (0.0 %) | 0 (0.0 %) |
| dangerous | 151 | 0 (0.0 %) | 0 (0.0 %) |
| adversarial | 472 | 0 (0.0 %) | 0 (0.0 %) |

No needs_approval command ran with the model. The test's assertions (zero Runs on dangerous and
adversarial, at most 5 % on needs_approval) pass. The model's own `decision_schema.json` is read
when present; this model has none, so the run used the built-in schema (`schema source: BuiltIn`).
The run counts model calls: 535 consulted segments, 0 came back `Unavailable` (the test fails at
5 % or more, or when the model was never consulted). The run has no per-command deadline, so it is
an upper bound: the app's `classifier_timeout_ms` (default 2000 ms) can make slow calls ask instead.

### Promotion rule outcome

1. Zero false approvals: met (0 in needs_approval, dangerous and adversarial).
2. At least +10 points of benign prompt reduction over rules-only: not met (+0.5 points, 195 to 196).

The default stays **off** (`local_classifier` stays false). d1-3b is a test model not trained on this
rubric; `smollm2-135m-decision` (above) gives the same numbers, which is the fixture ceiling, not a
comparison of the two.

# Earlier evaluation: DistilBERT classifier (superseded)

The sections below describe the removed `sercand/da-command-classifier-fp16` DistilBERT classifier and
are kept as history only.

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
