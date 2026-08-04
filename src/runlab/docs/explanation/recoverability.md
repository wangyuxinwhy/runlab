# Recoverability

RunLab promises that a Run stays usable as a control after the world around it moves. This document states what that promise is, why it is deliberately weaker than the obvious alternatives, and where it stops.

## Drift is an event, not a duration

"Will this still work in three months" is the wrong question, because it suggests decay is gradual and that recent work is safe.

An environment stops existing the moment any of these happens: a base image tag moves, a package manager resolves to a newer version, an install script fetches today's payload, a local image is pruned, or a build runs on a machine with a cold layer cache. The last one needs no time to pass at all — the same `Dockerfile` produces a different image on a colleague's laptop this afternoon.

None of these announce themselves. The declaration is byte-identical, so every digest computed from it matches, and the record shows two Runs sharing an environment they did not share.

## The cost of losing a baseline

Run environment A over ten Tasks today. Next week, try variant B.

If A's image is still retrievable, you run ten new Runs and compare. The cost of a variant is proportional to the variant.

If A has drifted, A's results describe a world that no longer exists. Comparing them to B's results is comparing across an uncontrolled change, so you must re-run A as well — and the twenty Runs you just produced will face the same problem when C arrives.

That is the difference between a Run that is an **asset** and a Run that is a **consumable**. An asset retains value and can be reused by later work; a consumable is meaningful only inside the batch that produced it. Nothing else in the design matters if Runs are consumables, because a body of experiments that cannot accumulate is just a sequence of one-off measurements.

## Why not rebuildability

The intuitive fix is to make declarations rebuild identically. Pin the base image by digest, pin every package version, snapshot the package index.

This is the deterministic-build problem, and Agent runtimes are close to its worst case. The CLI under test ships new versions constantly, and you frequently *want* a specific recent one. Pinning every transitive dependency is neither achievable nor, for this use, desirable.

More importantly it is unnecessary. You do not need to re-derive the environment. You need to obtain the one that ran.

Replacing rebuildability with **recoverability** turns a research problem into a storage problem — and a storage problem you can solve.

## How a realization is kept

A declaration is source. A realization is what was built from it, addressed by a platform-specific digest, and recorded in a lock file committed beside the declaration.

Resolution then has three outcomes, and the third is the entire point:

| State | Behavior |
| --- | --- |
| No lock | Build, write the lock |
| Lock present, realization retrievable | Use it |
| Lock present, realization missing | **Fail** |

The failing case is the one that protects you. A silent rebuild is how a control group changes while every digest in the record continues to match. Failing converts drift into something a person has to authorize with `--rebuild`, and the rebuilt realization is typically *not* identical to the one it replaced — which is precisely the evidence that the guard was doing real work.

Content addressing carries a second consequence here. A digest is only useful while the thing it addresses still exists; on its own it can prove that two things differ but cannot reconstruct either. That is why Run records archive byte copies of the declarations that produced them instead of merely recording their digests.

## The boundary

Stating a guarantee without stating its edge produces the same false confidence as no guarantee at all.

**Guaranteed recoverable:** declarations, built images, Task content and workspace, and declared local inputs.

**Recorded but not guaranteed:** model services, external APIs, and anything else reachable only over the network. A model behind a given name can change, and RunLab cannot hold it still.

**Never stored:** credentials. That exclusion is deliberate and permanent.

So the accurate claim is narrow: a Run re-executed later differs only through the parts explicitly marked unguaranteed. That is weaker than reproducibility, and it is enough — the remaining variance has a name and a location, which is all attribution requires.

## What this does not solve

Repeats are not part of RunLab. Estimating how much of a difference is noise takes several Runs of one configuration, and that is composition, which belongs to the caller.

What RunLab owes that caller is narrower and non-negotiable: when a script asks for the same environment twice, it must actually get the same environment. Recoverability is what makes that true.

See [Model](../reference/model.md) for lock semantics and identity, and [Design Principles](principles.md) for the full invariant set.
