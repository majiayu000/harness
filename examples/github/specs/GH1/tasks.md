# Task Plan

## Linked Issue

GH-1

## Spec Packet

- Product: `specs/GH1/product.md`
- Tech: `specs/GH1/tech.md`

## Implementation Tasks

- [ ] `SP1-T001` Owner: `docs` | Done when: the issue template, PR template, workflow check, and spec packet layout are copied into the adopting repository | Verify: `python3 checks/check_workflow.py --repo .`
- [ ] `SP1-T002` Owner: `maintainer` | Done when: the linked issue readiness state is recorded before implementation begins | Verify: reviewer checks the issue readiness section

## Parallelization

Template review and spec packet review can run in parallel because they touch
different files.

## Verification

- `python3 checks/check_workflow.py --repo .`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1`

## Handoff Notes

Replace the sample issue number, owners, and verification commands before using
this packet for real work.
