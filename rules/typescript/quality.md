---
paths: "**/*.ts,**/*.tsx"
---

# TypeScript Quality Rules

## TS-01: No `any` type in public APIs (high)
Use proper type annotations. `unknown` for truly unknown types.

## TS-02: No `console.log` residuals (medium)
Remove debug logs before committing. Use structured logging.

## TS-03: No empty catch blocks (high)
Handle or rethrow. Never silently swallow errors.

## TS-04: No duplicate string constants (medium)
Extract to a constants file.

## TS-05: No `as` type assertions without validation (medium)
Use type guards or runtime validation.

## TS-06: No `==` comparison (low)
Use `===` for strict equality.

## TS-07: No `var` declarations (low)
Use `const` by default, `let` when reassignment needed.

## TS-08: No barrel exports creating circular dependencies (high)
Check import graphs. Use direct imports.
