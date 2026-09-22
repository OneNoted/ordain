---
name: ordain-compile
description: Compile repository instruction files into an Ordain rubric, validate it, and calibrate it.
---

# Compile an Ordain rubric

Turn only the rules in the listed instruction files into the rubric named by
the caller. The judge sees the submitted questions, changes and configured evidence,
not the coding agent's full conversation or an independently browsable repository.
Do not add rules you think the project should have. The JSON is a
committed, hand-editable provenance record. Optional curated rules live separately in
`.ordain/presets.json`: do not copy, recompile or replace that snapshot when compiling
project instructions. `preset:*` sources are revision labels, not files to follow.

1. Read every listed source in full and inspect the existing rubric before replacing
   it. Follow explicit instructions to read or follow a local linked style guide or
   instruction file, even when the referring file also contains other instructions.
   Resolve relative links from that file, deduplicate targets and stop at cycles.
   List each followed instruction file as a source with the referring scope; do not
   widen nested scope. Do not crawl background references, example links or remote URLs.
   Previously recorded sources remain part of a refresh unless removed or explicitly
   retired. Keep existing rule IDs when their meaning is unchanged, so project and
   per-rule configuration continues to address the same rules. Do not silently drop
   sources or rules merely because they are not named AGENTS.md or CLAUDE.md.
   Nested files apply only to the scope shown. Read a listed CONTRIBUTING.md
   only for imperative code instructions. Skim listed lint configs only to
   identify overlaps.

2. Extract every statement that tells the coding agent to do or not do
   something about code. Keep the user's wording in `text` (one or two
   sentences) and record `source.path` and the one-based `source.line`. Do not
   merge separate rules or split one idea merely because it has clauses.

3. Put each instruction in exactly one bucket, in this order:

   - `lint`: syntax a linter can enforce exactly. Give `how` and optionally a
     grep-shaped `pattern`; Ordain records this but does not run it.
   - `deferred`: counting/measuring needs a script, or the answer needs repository
     context unavailable in the configured evidence. Give a concrete `reason`.
   - `model`: a judge can answer from the changes and configured evidence. Do not
     assume whole-file or related-file context when the project supplies only diffs.
   - `unenforceable`: conversation/process behavior rather than code. Give a
     concrete `reason`.

4. Separate the human policy from its classification question. Keep the policy's
   wording in `text`; write `question` for a classifier, not a coding assistant.
   Ask one concrete violation predicate in `instructions`, normally under 60 words.
   Avoid general coaching ("write high-quality code", "think carefully") and inferred
   motives ("merely to work around"). Describe observable behavior or contracts.
   Use:

   - `boolean` for existence of a violation: `true` MUST mean that the rule
     is broken, and `false` that no violation is evidenced. Ordain uses the
     probability of `true` as its violation confidence. Never ask whether the
     change complies, follows the policy, or uses the preferred approach.
     Put explicit decision boundaries in `criteria.true` and `criteria.false`, not
     a paragraph of exceptions in `instructions`. True states the violation;
     false covers compliance, relevant exceptions and absence of the relevant code.
     Use contrastive examples when similar syntax has different meanings: copying
     unchanged data only to inspect it versus creating a representation required by
     an API; asserting a literal against itself versus comparing real output with a
     fixed expected literal. Examples illustrate the predicate, not a whitelist of
     function names or APIs. Do not widen or narrow the source rule to fit a fixture.
   - `choice` for a closed set: `criteria` maps names to descriptions and
     `violating` lists the bad names, leaving at least one compliant name.
   - `score` for degree: `criteria` is ordered compliant-to-worst and
     `violatingFrom` is the first bad zero-based index.

   Put worked examples in criteria. Do not put file scope in question text.
   Before writing the rubric, answer each question against a violation, a compliant
   change and a legitimate exception. Only the violation should select a violating
   outcome. This semantic check is required even when JSON validation passes.
   When comparing formulations using the judge, freeze labels, evidence and
   thresholds; test more than the examples named in the question. Inspect findings
   from all applicable rules, not just the target. A concise predicate, explicit
   criteria or contrastive examples can each work; schema validity and higher
   confidence on violations alone do not establish better classification.

5. Give each model rule `when: "edit"` if one edit hunk is sufficient, or
   `when: "turn"` if a reviewer needs the whole change (scope creep, overall
   size, single-use abstraction, whether a module was needed). Lint rules do
   not need `when`.

6. Write plain JSON to the exact target path:

```json
{
  "version": 1,
  "compiledAt": "2026-09-19T00:00:00Z",
  "compiledBy": "claude",
  "sources": [{ "path": "AGENTS.md", "scope": "**/*" }],
  "rules": [
    {
      "id": "no-interface",
      "text": "Use type, never interface.",
      "source": { "path": "AGENTS.md", "line": 10 },
      "scope": ["**/*.ts", "**/*.tsx"],
      "check": { "type": "lint", "how": "@typescript-eslint/consistent-type-definitions", "pattern": "^\\s*interface\\s" }
    },
    {
      "id": "raw-error-to-user",
      "text": "Never show a user a raw error.",
      "source": { "path": "AGENTS.md", "line": 20 },
      "when": "edit",
      "check": {
        "type": "model",
        "question": {
          "type": "boolean",
          "instructions": "Does this change put raw exception text into a response or rendered user-facing message?",
          "criteria": { "true": "String(error) in a response body", "false": "a fixed human-authored message" }
        }
      }
    }
  ]
}
```

IDs are unique kebab-case. Every rule source must appear in `sources`. Use
actual compilation time for `compiledAt`. `compiledBy` is optional: identify
only the host/model actually used, or omit it rather than copying the example.
Leave source `sha`, rule `status`, and `calibration` out: the CLI owns those fields.
Do not write any other file.

7. Run the exact CLI invocation supplied by the caller:

```
ordain rubric validate
ordain config validate
ordain calibrate
```

Add `--global` to rubric validation and calibration when compiling the global rubric.
Run config validation in the project. Fix every validation issue; an orphaned rule
override can indicate an accidentally dropped or renamed rule, not obsolete config.
Calibration may skip without history. If it marks a rule weak/noisy, rewrite
only those questions once, then validate and calibrate once more; do not loop.

8. Report in two or three sentences how many rules landed in each of lint,
model, deferred, and unenforceable, plus weak/noisy rules. Then resume the
user's request.
