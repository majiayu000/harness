# Stable-ID Conformance Vectors

## Linked Issue

GH-1734

These vectors are normative inputs to the byte grammar in `tech.md`. Tests must
decode the literal canonical hex and hash those bytes independently of the
production encoder.

## Empty Observed Snapshot

- Schema: `agent-stack-snapshot/v0.1`
- Coverage, in closed rank order: all four domains `observed`
- Entries: zero
- Canonical length: 251 bytes
- SHA-256:
  `a70ef74bf084fba3e6d0d12daeebc09b24236ffe76d601a85f89cdc4f1106200`

## Repository and Runtime-Context Snapshot

Coverage is all four domains `observed`; the runtime- and MCP-fingerprint
collections are successful and empty. Canonical entries use this exact
component-ID order:

1. `repository:instructions:AGENTS.md`
2. `runtime:memory:repo_memory/record-00000000-0000-4000-8000-000000000001`

On Unix, the repository evidence is the exact shape emitted by the current
ASC-002 inventory for `AGENTS.md`: a `regular_file` with executable tag `0x02`,
integrity `SHA-256("repo")`, and an empty capability list. The portable vector
constructs that same shape from a typed ASC-002 entry fixture. The context
evidence has `SHA-256("context")` component integrity, semantic order zero, reason
`repo_memory_selected`, and present metadata: canonical UUID record ID
`00000000-0000-4000-8000-000000000001`, present evidence reference
`artifact:7`, and estimated tokens 42.

The exact intermediate digests are:

| Input | Lowercase SHA-256 |
| --- | --- |
| `repo` | `071ca2227754705837aa3ef9748ed59e9f8a015fd765c42f391a4cbc271c6d5e` |
| `context` | `ea7792a26f405e2ae9c6f49ca93bbe6076ceac0a1fc53d83426c7d7f2d9377e4` |

The successful literal vector is portable: its repository component is built
from a typed `AgentStackInventoryEntry` fixture whose executable tag is
`0x02`, together with the closed GH-1734 context input and no placeholder
fingerprint envelopes. The implementation test must require exact equality
with the literal bytes below and then hash the literal independently. On Unix,
a separate integration test must construct the same repository component
through the real ASC-002 inventory and consume it through the crate-visible
`into_entries(self)` API. On non-Unix targets the real inventory emits
`unix_executable: None` and therefore is not required to reproduce `0x02`.
Separate branch-matrix tests cover the `None` case, executable tags
`0x00`/`0x01`, directory presence, absent context metadata/reference,
`not_observed` coverage, and GH-1733's complete valid runtime/MCP envelope
vectors.

## Typed Context Conformance Cases

The fixed successful bytes below use `repo_memory_selected`. Separate typed
fixtures cover all six producer shapes: runtime profile (both the valid
historical locator and GH-1732 hash fallback), central workflow source,
repository workflow override, effective workflow document, default workflow
document, and selected repository memory. For each row, changing only kind,
scope, or locator fails `invalid_context_metadata` and produces no canonical
bytes or stable ID.

An exact duplicate of one valid context item fails
`duplicate_component_evidence` before global semantic-order validation. Two
distinct valid context items sharing one order fail `inconsistent_observation`.
Reversing either rejected input vector preserves its error category. These are
typed rejection vectors and do not change the successful canonical length,
literal hex, or digest below.

For one `(component_id, evidence_kind)` bucket, `A, A`, `A, A, A` are
`duplicate_component_evidence`. The three unique permutations `A, A, B`,
`A, B, A`, and `B, A, A` are all `inconsistent_observation`; distinct canonical
bytes take precedence over exact-duplicate multiplicity.

Canonical length: 1,312 bytes.

Canonical input hex:

```text
6861726e6573735f6167656e745f737461636b5f736e617073686f745f69645f76305f310000000000000000196167656e742d737461636b2d736e617073686f742f76302e31000000000000000400000000000000147265706f7369746f72795f696e76656e746f727900000000000000086f62736572766564000000000000000f72756e74696d655f636f6e7465787400000000000000086f62736572766564000000000000001372756e74696d655f66696e6765727072696e7400000000000000086f62736572766564000000000000000f6d63705f66696e6765727072696e7400000000000000086f62736572766564000000000000000200000000000001b300000000000000217265706f7369746f72793a696e737472756374696f6e733a4147454e54532e6d640000000000000001000000000000017a00000000000000147265706f7369746f72795f696e76656e746f7279000000000000001a6167656e742d737461636b2d636f6d706f6e656e742f76302e3100000000000000217265706f7369746f72793a696e737472756374696f6e733a4147454e54532e6d64000000000000000c696e737472756374696f6e73000000000000000a7265706f7369746f727900000000000000094147454e54532e6d6400000000000000137265706f7369746f72795f6f62736572766564000000000000000a646973636f766572656401000000000000004030373163613232323737353437303538333761613365663937343865643539653966386130313566643736356334326633393161346362633237316336643565000000000000000000000000000000137265706f7369746f72795f6f627365727665640000000000000005667265736800000000000000197265706f7369746f72795f696e76656e746f72792f76302e31000000000000000c726567756c61725f66696c65020000000000000262000000000000004672756e74696d653a6d656d6f72793a7265706f5f6d656d6f72792f7265636f72642d30303030303030302d303030302d343030302d383030302d30303030303030303030303100000000000000010000000000000204000000000000000f72756e74696d655f636f6e74657874000000000000001a6167656e742d737461636b2d636f6d706f6e656e742f76302e31000000000000004672756e74696d653a6d656d6f72793a7265706f5f6d656d6f72792f7265636f72642d30303030303030302d303030302d343030302d383030302d30303030303030303030303100000000000000066d656d6f7279000000000000000772756e74696d6500000000000000377265706f5f6d656d6f72792f7265636f72642d30303030303030302d303030302d343030302d383030302d303030303030303030303031000000000000001072756e74696d655f6f6273657276656400000000000000066c6f61646564010000000000000040656137373932613236663430356532616539633666343963613933626265363037366365616330613166633533643833343236633764376632643933373765340000000000000000000000000000000d73656c665f6465636c6172656400000000000000056672657368000000000000001472756e74696d655f636f6e746578742f76302e31000000000000000000000000000000147265706f5f6d656d6f72795f73656c656374656401000000000000002430303030303030302d303030302d343030302d383030302d30303030303030303030303101000000000000000a61727469666163743a37000000000000002a
```

Expected SHA-256:

```text
da375d4cf97e7b01281a18130dacc614706aec719b7611320aa1ccb6b846f49e
```
