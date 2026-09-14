# Review quality eval

This small evaluator-owned catalog measures review decisions before changing
production prompts, policies, or workflow transitions. It is not an executable
Issue replay manifest: importing it into the runtime benchmark driver would
misrepresent its scope. All cases use frozen Harness source at `d1624586`.

## Cases

| Pair/case | Distinction being evaluated |
| --- | --- |
| R01 / R02 | Same inherited-definition behavior; explicitly prohibited combination versus explicitly required project override |
| R03 / R04 | Same validation merge; central defaults intentionally retained versus explicitly excluded |
| R05 | Precedence unspecified: distinguish observable behavior from an unsupported blocker |
| R06 | Unrelated feedback-loop control: assess external advice rather than restoring a rejected count heuristic |

R02 and R04 are **counterfactual requirements** applied to real source, not new
requirements for Harness. Passing their review cases does not authorize changing
production precedence. R01/R03 check false positives, R02/R04 check misses,
R05 checks uncertainty, and R06 checks external-review deference.

The oracle is source-traced, not newly runtime-reproduced. These six cases are
development cases; no unseen held-out result
or general model-quality claim is available. Reserve a separate future case set
before tuning and evaluating any proposed prompt change.

The [first Grok R01/R02 run](runs/grok-r01-r02-20260914.json) attempted both
cases once. Neither produced a final review: both reached the one-turn limit
after recording tool activity despite the requested empty tool allowlist.
The run has zero semantically evaluable cases, not two failed review judgments.
It consumed 55,203 CLI-reported tokens across two model calls. No automatic
rerun occurred; these incomplete attempts remain recorded.

The [authorized second attempt](runs/grok-r01-r02-20260914-attempt2.json)
corrected the CLI tool selection without changing candidate inputs. A separate
preflight and both case sessions persisted empty tool definitions. Nevertheless,
both reviews timed out at 240 seconds. R01's exported assistant text repeated
the same progress phrase 127 times; R02 exported no assistant review. Completion
was 0/2 for this attempt; review precision and recall remain unmeasurable.
Preflight usage was 12,963 tokens; usage for both interrupted reviews is unknown.
The tool boundary is now evidenced, but this direct CLI setup has not established
reliable review completion. No production rule change follows from these results.

## Offline preparation

From the repository root:

```sh
python3 evals/review-quality/prepare.py --check
python3 evals/review-quality/prepare.py --case R02 --output /tmp/harness-review-r02
```

The first command validates frozen source references, not semantic correctness.
The second prepares exactly one standalone prompt and its digest. It does not
clone a repository, call a model, run a server, or change a working tree. Existing
output directories are rejected so a previous attempt cannot be overwritten.

Give an agent only the prepared prompt, in a fresh context with no tools and no
access to this catalog, its oracle, the paired case, or previous review results.
Do not send the catalog, README, or input manifest to the model. The visible task
requirements are intentional; the expected findings and grading are withheld.
Input preparation does not itself provide filesystem sandboxing.

Keep the prompt, model/settings, CLI version, source revision, raw final answer,
actual call/turn count, tokens, elapsed time, and evaluator decision for each
attempt. Direct CLI trials must be labeled as such; they do not establish
native Harness-to-Grok integration. Start with one pair in separate sessions,
concurrency one, and a single turn per case. No automatic repair or rerun follows
a finding. The previous Grok trial is exploratory, not a comparable baseline.

## Grading before prompt changes

1. Match each claimed blocker to the stated requirement, exact code path, and
   trigger. A correct line reference without a requirement violation is not a
   true positive. Optional design criticism is allowed and is not a false
   positive unless presented as necessary for correctness or completion.
2. R02/R04 each have one expected defect. Credit requires the trigger and actual
   versus expected behavior, not just a verdict word. R01/R03 have no expected
   blocker within the narrow case scope. R05 must identify the missing
   precedence decision without inventing one. R06 must assess the external
   proposal against the explicit requirement rather than accepting it on rank.
3. Do not automatically reject an unlisted finding: independently reproduce or
   trace it and adjudicate it before scoring. Keep disputed cases inconclusive.
   A timeout, missing answer or unavailable environment is incomplete, not a
   clean review. Record scope expansion or unauthorized actions separately.
4. Report true positives, false positives, misses, and evaluable denominators.
   Precision = TP / (TP + FP); recall = TP / (TP + FN). Empty denominators are
   N/A. Report no-defect-case accuracy, uncertainty handling and external-advice
   disposition separately. Semantic matching is human-adjudicated, not an
   automatic keyword score or a second model's unexplained number.

The development acceptance target is both expected defects found, no unsupported
blockers in the controls, correct uncertainty/advice disposition, and no scope
expansion. This is a small-suite target, not a production release certificate.
Report first-attempt case outcomes separately from retries. A single execution
cannot establish repeated-run reliability (`pass^k`) or a credible population
success rate. Compare before/after variants on the same frozen inputs only after
this baseline exists. No new runtime guard or instruction rule is added here.
