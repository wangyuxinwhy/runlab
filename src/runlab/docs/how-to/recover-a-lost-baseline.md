# Recover a Lost Baseline

A Run fails before it starts with a message like this:

```text
runlab: Overlay 'with-python' is locked to a realization that is no longer available.
Rebuilding produces an environment that is not comparable with earlier Runs;
pass --rebuild to accept that.
```

The lock names an image that is no longer on this machine. RunLab stops instead of rebuilding, because a silent rebuild changes a control group while every digest in the record continues to match.

Exit status is `1` and no Run directory is created — the failure happens before acceptance, so there is nothing partial to clean up.

## Retrieve the original first

Rebuilding is the last option, not the first. Check whether the image still exists somewhere:

```bash
docker image inspect sha256:d7a3eba1a9e8994b771407fef8ed19fe06f4ec204646de3d2e2d6634ca612ea7
docker pull <registry>/<repo>@sha256:d7a3eba1…       # if you push realizations
```

Anything that puts that exact image back on the machine resolves the failure with no loss of comparability. This is the reason to push realizations to a registry rather than leaving them only in a local image store.

## Rebuild only when you accept the consequence

```bash
uv run runlab overlay build overlays/with-python --base bases/pi --rebuild
```

The new realization is generally **not** identical to the one it replaced — a package index moved, a base layer changed, a version resolved differently. That is the point of the guard: what looked like a recoverable inconvenience is actually a new environment.

`--rebuild` is also accepted by `run start`, which resolves and rebuilds in one step.

## Handle the results honestly

Runs recorded against the old realization are still valid records of what happened. They are no longer valid **controls** for Runs made against the new one, because the environment digests differ and nothing establishes what else changed with them.

Re-run the arms you intend to compare against the new realization. Do not mix the two sets in one comparison — that is exactly the uncontrolled change the layering exists to prevent.

## Reduce how often this happens

Push each realization to a registry as soon as it is built, and keep the lock files in version control beside their declarations. A lock without a retrievable image is a receipt for something you no longer have.

Pinning base images and Agent CLI versions reduces how often a rebuild produces something different, but it does not remove the need to keep the built image. Pinning is hygiene; storage is the guarantee. See [Recoverability](../explanation/recoverability.md) for why the distinction matters.
