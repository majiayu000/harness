---
paths: "**/*.go"
---

# Go Quality Rules

## GO-01: No discarded errors (critical)
Never `_ = someFunc()` for error returns. Handle or wrap.

## GO-02: No goroutine leaks (high)
Every goroutine must have a termination path via context or channel.

## GO-03: No `defer` in loops (high)
Deferred calls accumulate per function. Extract loop body to helper.

## GO-04: No `interface{}` / `any` without type assertion (medium)
Use generics or concrete types where possible.

## GO-05: No `panic` in library code (high)
Return errors instead. `panic` only in `main()` for truly unrecoverable states.

## GO-06: No shared memory without synchronization (critical)
Use channels or `sync.Mutex`.

## GO-07: No oversized structs passed by value (medium)
Pass structs > 64 bytes by pointer.

## GO-08: No init() for non-trivial logic (medium)
Use explicit initialization functions.
