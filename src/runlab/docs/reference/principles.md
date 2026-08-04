# Design Principles

RunLab records reproducible Agent execution facts through one fixed operation:

```text
Base + Overlay + Task -> Run
```

This document states what RunLab guarantees, what it refuses, and the judgment used to admit anything new. [Model](model.md) derives the entities from these principles, and [Architecture](architecture.md) derives the code layering.

## The Scarce Property Is Comparability

Executing an Agent is not scarce. A shell script and a container image do it. What is scarce is the guarantee that **two Runs differ only in the ways they were declared to differ**.

Every other principle follows from this one. The operation is fixed because an arbitrary command is an unrecorded variable. Identity is content-addressed because "the same Base" must be decidable rather than nominal. Credentials collapse into opaque slots because they are the one input that must exist without influencing identity. The Task workspace is deleted after collection because it is intermediate state that invites being treated as evidence while carrying the highest variance of anything in the Run.

## An Environment Is a Constant, Not a Function

Experiments require controlled variables. If part of the input changes without the operator knowing, the observed difference in output means nothing.

A directory containing a `Dockerfile` is not a constant. It is a function that may evaluate differently on every call: base image tags move, package managers resolve to new versions, `curl | sh` fetches whatever is served today, and layer caching makes the same file produce different images on a machine with a cold cache. Drift is triggered by events, not by elapsed time — a colleague's CI run can invalidate yesterday's baseline.

RunLab therefore separates the declaration from its realization. A declaration is source. A realization is the built artifact, addressed by digest, recorded in a lock file, and stored where it can be retrieved again. Runs reference realizations. Fixing a realization is an explicit act, and losing one is an explicit failure rather than a silent rebuild.

## A Run Is an Asset, Not a Consumable

An asset retains value over time and can be reused by later work. A consumable is meaningful only in the moment it was produced.

When environments drift, every new variant forces re-running all controls, because the old results were produced in a world that no longer exists. Incremental experiment cost degrades from proportional-to-new-work to proportional-to-everything, and it never converges. Preserving realizations is what keeps historical Runs usable as controls, which is the difference between a body of experiments that accumulates and one that restarts.

## Recoverability, Not Reproducibility

Guaranteeing that a declaration rebuilds bit-identically is a deterministic-build problem, and Agent runtimes are the worst case for it: the CLI under test ships new versions weekly, and pinning every transitive dependency is neither achievable nor desirable.

RunLab targets the weaker but sufficient property. It does not promise to re-derive an environment; it promises to retrieve the one that ran. That reduces a research problem to a storage problem.

The boundary is explicit. RunLab guarantees that locally controllable inputs — declarations, built images, Task content and workspace, declared inputs — are recoverable byte for byte. It records but does not guarantee model services, external APIs, and anything else reachable only over the network. It never stores credentials. A Run that differs on re-execution differs only through the parts marked unguaranteed.

## Facts, Not Judgments

RunLab records execution facts. It does not score, rank, or select a winner, and it does not decide what counts as a good artifact.

Normalizing model usage across runtimes is the one place this line is under pressure, and it is resolved by a stronger rule rather than by refusing to normalize: **every derived value must be reconstructible from retained evidence**. Raw stdout, raw runtime sessions, and raw measurement samples are preserved, so a normalized token count is a convenience rather than a replacement. A derivation that cannot be re-run against retained evidence is not admissible.

## Invariants by Rigidity

Treating all invariants as equally rigid is as wrong as treating none of them that way. These three tiers carry different obligations.

### Load-bearing

Changing one of these produces a different product.

The operation is fixed. RunLab never accepts an arbitrary command.

An accepted Run always retains a terminal record, including setup, execution, and collection failures. Silent loss of failed samples is survivorship bias that no downstream analysis can detect or repair.

Every derived value is reconstructible from retained evidence.

A realization is immutable and addressable. Two Runs are comparable only when their full realization chain is identical.

Credentials never enter images, public records, logs, workspaces, or artifacts.

### Boundary policies

These are current scope, not doctrine, and may be relaxed without disturbing the tiers above: Docker as the only container engine, one container per Run, one compact JSON object on stdout, and the absence of any orchestration surface.

### Unverified assumptions

These currently behave like invariants but have no evidence supporting their permanence. Each names a real product direction rather than a hypothetical risk.

A Task carries one initial instruction. Multi-turn or scripted interaction has no place in the model yet; the stdin contract belongs to a named `single-turn` interaction protocol rather than to Base itself, so a second protocol can be added without breaking existing declarations.

One Run is one container. A Task needing a live service alongside the Agent — a database, a mock API, a target application — does not fit.

Artifacts and logs are partitioned by origin. Evaluating an Agent's process rather than its deliverable makes logs the object of evaluation, which the partition permits but the naming has historically obscured.

## What RunLab Does Not Do

Selection, repeats, ordering, adaptive scheduling, budget allocation, and judging stay in Agent-authored scripts. RunLab exposes one execution entry point and leaves composition to its caller.

It does not model arbitrary container topologies, and it will not adopt an orchestration DSL to obtain them.

It does not pursue determinism through seeds or network record-and-replay.

It does not compute statistical inference over Runs. Recording a distribution is a fact; testing a hypothesis is a judgment.

## Admitting Something New

Two questions gate every proposed entity, field, or command.

Is it an independent dimension of the input, or a derived view of the record? A grouping that can be computed from Run records needs no entity. An input dimension that is currently flattened into another layer needs one.

Does a real experiment demonstrate the need? Evidence precedes admission. A plausible future use is not evidence.
