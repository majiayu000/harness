---
paths: "**/*.py"
---

# Python Quality Rules

## PY-01: No mutable default arguments (critical)
Use `None` with factory pattern: `def f(items=None): items = items or []`

## PY-02: No bare `except:` (high)
Catch specific exceptions. At minimum `except Exception:`.

## PY-03: No `pickle` on untrusted data (critical)
Use `json` or `yaml.safe_load()`.

## PY-04: No `import *` (medium)
Explicit imports only.

## PY-05: No global mutable state (medium)
Use dependency injection or context managers.

## PY-06: No f-strings in logging (low)
Use `logger.info("msg %s", var)` for lazy formatting.

## PY-07: No missing type annotations on public functions (medium)
All public function signatures must have type annotations.

## PY-08: No `assert` for runtime validation (medium)
Asserts are removed with `-O` flag. Use explicit checks.
