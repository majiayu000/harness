# GH-1734 Conformance Vectors

Status: deferred; no vectors are normative.

The former canonical byte strings and digests described a superseded snapshot
contract coupled to the removed GH-1733 envelope. They remain available in
version history as research only.

New vectors must be written after a production consumer defines the minimum
stable projection. They must independently cover:

- representation-only reordering that preserves identity;
- every consumer-declared behavior fact changing identity;
- volatile observation metadata preserving identity;
- missing, failed, and successful-empty observations remaining distinct;
- malformed or over-limit input producing no stable ID.
