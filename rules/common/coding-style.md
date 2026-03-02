# Common Coding Style Rules

## CS-01: No public API signature changes without explicit request (high)
Breaking changes require user confirmation and MAJOR version bump.

## CS-02: No premature abstractions (high)
Wait for the third repetition before extracting. 3 lines of duplication > 1 premature abstraction.

## CS-03: No unrequested features (high)
Bug fix scope is strictly locked. Do not refactor surrounding code.

## CS-04: No guessing user intent (high)
When uncertain, mark as DEFER or ask.

## CS-05: No style changes mixed with fixes (medium)
Style changes belong in separate commits.

## CS-06: Immutability preferred (medium)
Create new objects rather than mutating. Function parameters are read-only.

## CS-07: File size control (medium)
200-400 lines typical, 800 lines maximum. Split above 800.
