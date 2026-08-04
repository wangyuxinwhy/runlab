# Design Principles

RunLab records reproducible Agent execution facts through one fixed operation:

```text
Base + Overlay + Task -> Run
```

This document states what RunLab guarantees, what it refuses, and the test used to admit anything new. [Why the layers exist](why-layers.md) derives the declaration kinds, [Recoverability](recoverability.md) derives the storage guarantee, and [Model](../reference/model.md) specifies the resulting formats.

## The scarce property is comparability

Executing an Agent is not scarce; a shell script and a container image do it. What is scarce is the guarantee that **two Runs differ only in the ways they were declared to differ**.

Most of the design follows from that one sentence. The operation is fixed because an arbitrary command is an unrecorded variable. Identity is content-addressed because "the same Base" must be decidable rather than nominal. Credentials collapse into opaque slots because they are the one input that must exist without influencing identity. The Task workspace is deleted after collection because it is intermediate state that invites being treated as evidence while carrying the highest variance of anything in the Run.

A declaration is separated from the realization built from it for the same reason, and a Run archives byte copies of its declarations because a digest proves that two things differ without reconstructing either.

## Facts, not judgments

RunLab records execution facts. It does not score, rank, or select a winner, and it does not decide what counts as a good artifact.

Normalizing model usage across runtimes is the one place this line comes under pressure, and it is resolved by a stronger rule rather than by refusing to normalize: **every derived value must be reconstructible from retained evidence**. Raw stdout, raw runtime sessions, and the archived declarations are all preserved, so a normalized token count is a convenience rather than a replacement. A derivation that cannot be re-run against retained evidence is not admissible.

## Invariants by rigidity

Treating all invariants as equally rigid is as wrong as treating none of them that way. These tiers carry different obligations.

### Load-bearing

Changing one of these produces a different product.

The operation is fixed. RunLab never accepts an arbitrary command.

An accepted Run always retains a terminal record, including setup, execution, and collection failures. Silent loss of failed samples is survivorship bias that no downstream analysis can detect or repair.

Every derived value is reconstructible from retained evidence.

A realization is immutable and addressable, and a missing one fails the Run. Two Runs are comparable only when their full realization chain is identical.

Credentials never enter images, public records, logs, workspaces, or artifacts.

### Boundary policies

These are current scope rather than doctrine, and may be relaxed without disturbing the tier above: Docker as the only container engine, one container per Run, one compact JSON object on stdout, and the absence of any orchestration surface.

### Unverified assumptions

These behave like invariants but have no evidence supporting their permanence. Each names a real product direction rather than a hypothetical risk.

A Task carries one initial instruction. Multi-turn or scripted interaction has no place in the model yet, so the stdin contract belongs to a named `single-turn` interaction protocol rather than to Base itself, and a second protocol can be added without breaking existing declarations.

One Run is one container. A Task needing a live service alongside the Agent — a database, a mock API, a target application — does not fit.

Artifacts and logs are partitioned by origin. Evaluating an Agent's process rather than its deliverable makes logs the object of evaluation, which the partition permits but the naming has historically obscured.

## What RunLab does not do

Selection, repeats, ordering, adaptive scheduling, budget allocation, and judging stay in Agent-authored scripts. RunLab exposes one execution entry point and leaves composition to its caller.

It does not model arbitrary container topologies, and it will not adopt an orchestration DSL to obtain them.

It does not pursue determinism through seeds or network record-and-replay.

It does not compute statistical inference over Runs. Recording a distribution is a fact; testing a hypothesis is a judgment.

## Admitting something new

Two questions gate every proposed entity, field, or command.

Is it an independent dimension of the input, or a derived view of the record? A grouping that can be computed from Run records needs no entity. An input dimension that is currently flattened into another layer needs one.

Does a real experiment demonstrate the need? Evidence precedes admission, and a plausible future use is not evidence.
