"""The ordered realization chain and the comparability key folded from it."""

from collections.abc import Mapping, Sequence

from runlab.core.digest import digest_values


def environment_digest(
    base_realization: str,
    /,
    *,
    overlay_realizations: Sequence[str],
    env: Mapping[str, str],
    network: str,
) -> str:
    """Fold what actually ran into one value.

    Two Runs are comparable exactly when this digest matches, so every element
    that can change Agent behavior enters it, and Overlay order is preserved
    rather than sorted: stacking is not commutative.
    """
    parts = ["base", base_realization, "overlays", *overlay_realizations, "env"]
    parts.extend(f"{name}={value}" for name, value in sorted(env.items()))
    parts.extend(["network", network])
    return digest_values(*parts)
