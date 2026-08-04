# Why the Layers Exist

RunLab splits a declaration into Base, Overlay, and Task. The split is not organizational tidiness; it is what makes a difference between two Runs attributable to something you chose.

## The requirement

An experiment varies one thing and observes the result. That only works if everything else is held fixed *and* if the thing you varied is identifiable afterwards from the record alone.

The second half is the one that gets lost. A record can hold every input and still fail to answer "what did I change", if the change had nowhere of its own to live.

## What goes wrong with two layers

Consider two ordinary experiments. One compares two versions of a skill the Agent loads. The other compares two versions of a CLI tool the Agent invokes.

These are the same kind of experiment: both vary what the Agent is able to do. But with only Environment and Task, they are forced apart by an irrelevant detail — how the thing is delivered.

A skill is a file, so it goes in the Task workspace. Now the Task digest has changed, and the record says you ran a **different task**.

A CLI tool is a binary, so it goes in the Dockerfile. Now the Environment digest has changed, and the record says you ran a **different environment**.

In both cases the record is accurate and useless. Nothing in it says "capability configuration changed", because the model had no place to say it. The layering was organized by *delivery mechanism* when the experiment needed it organized by *the role the change plays*.

## The three roles

| Layer | Answers | Changes |
| --- | --- | --- |
| Base | Which Agent runs, and how it is invoked | Rarely |
| Overlay | What that Agent is configured to be able to do | Often |
| Task | What to do, and with what material | Often |

The dividing line between Base and Overlay is the execution contract. Swapping one Agent CLI for another changes the entrypoint, the output protocol, and where native logs land — that is a Base. Installing a tool the Agent can call, or handing it a different skill, leaves all of that intact — that is an Overlay.

The dividing line between Overlay and Task is reuse. Capability that several Tasks share is an Overlay; material that serves one instruction is Task content.

## Frequency is the other half of the argument

Base changes rarely and costs a full rebuild. Overlay changes constantly and should cost almost nothing.

Folding capability into the Base makes every skill edit rebuild the whole image, which is merely slow. The real damage is that the Base digest moves, and **every Run ever recorded against the old digest starts looking incomparable** — not because the Agent runtime changed, but because a line of instructions did. Historical Runs stop working as controls, which is the same failure as environment drift, arrived at deliberately.

## Four delivery forms, one layer

Having rejected delivery mechanism as the basis for layering, an Overlay must accept every mechanism a capability can arrive through.

| Form | Example | Realization |
| --- | --- | --- |
| Layer | Install a CLI tool; remove an interpreter | Image digest, depends on the Base |
| Mount | Skills, instructions | Content digest, independent of the Base |
| Env | Model selection, tool allowlists | Part of the environment key |
| Capability | Network access | Part of the environment key |

The env form is what keeps model selection from becoming homeless. Switching models is neither image content nor a mounted file, and it is unmistakably a change in what the Agent can do. It arrives as an environment variable that the Base entrypoint consumes, which leaves the execution contract owned by the Base — an Overlay never names a runtime's command-line flag.

The capability form is why `network` is not a policy setting. Policy carries resource bounds and termination conditions; running out of memory exhausts a resource, while losing network access removes an ability. A variable under study has to sit with the other capability variables, or the record scatters the difference again.

## Why an empty Overlay disappears

Declaring an Overlay that changes nothing and declaring no Overlay describe the same environment. Keeping both would put two declarations behind one realization, and comparability is judged on realizations.

So an empty Overlay normalizes away, and the Run record shows an empty chain either way. The archived declarations follow the same rule, so a normalized Overlay never appears to have been applied.

## What this buys

An ablation over one Task now reads directly from the records: the Base realization is identical, the Task digest is identical, and the Overlay chain is the only field that moved. That is what "attributable" means in practice — not that the system tells you why the results differ, but that it leaves exactly one candidate.

See [Model](../reference/model.md) for the declaration formats and [Recoverability](recoverability.md) for what keeps a realization retrievable long enough to matter.
