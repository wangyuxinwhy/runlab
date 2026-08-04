---
layout: home

hero:
  name: RunLab
  text: Execution facts that stay comparable
  tagline: One fixed operation — Base + Overlay + Task → Run — and a record that is still usable as a control long after it was produced.
  actions:
    - theme: brand
      text: Your first Run
      link: /tutorial/your-first-run
    - theme: alt
      text: Why the layers exist
      link: /explanation/why-layers
    - theme: alt
      text: Model reference
      link: /reference/model

features:
  - title: Declarations become constants
    details: A directory holding a Dockerfile is a function that evaluates differently on every call. RunLab freezes it into a realization, records the mapping in a lock file, and fails a Run rather than silently rebuilding a lost baseline.
  - title: One variable, one layer
    details: Base owns the Agent runtime, Overlay owns capability configuration, Task owns the instruction. A skill edit never moves the Base digest, so historical Runs stay usable as controls.
  - title: A Run is an asset
    details: Every accepted Run keeps a terminal record, archives byte copies of the declarations that produced it, and reports token usage as a first-class result rather than a footnote.
  - title: Composition belongs to you
    details: No matrix runner, no scheduler, no judging. RunLab exposes one execution entry point; repeats, selection, and evaluation live in your own scripts.
---

## The problem this solves

You run an experiment today with environment A. Next week you want to compare it against environment B. If A has drifted — a moved base image tag, a package manager resolving to a new version, a cold build cache — then A's old results were produced in a world that no longer exists, and comparing them to B means nothing. You have to re-run A too.

Do that enough times and the cost of each new variant stops being proportional to the new work and becomes proportional to everything you have ever run. Experiments stop accumulating.

RunLab separates what you *declare* from what actually *ran*, addresses the latter by digest, and refuses to lose it quietly. See [Recoverability](/explanation/recoverability) for what that guarantees and where the guarantee stops.

## Where to start

**New here?** [Your first Run](/tutorial/your-first-run) gets a real Agent executing in about ten minutes, then [your first ablation](/tutorial/your-first-ablation) shows the point of the layering.

**Evaluating the design?** [Why the layers exist](/explanation/why-layers) derives the three declaration kinds from the requirement that a difference between two Runs be attributable.

**Integrating it?** [Model](/reference/model) is the source of truth for declarations, identity, and locks. The same documents ship inside the package:

```bash
runlab docs list
runlab docs get reference/model
```
