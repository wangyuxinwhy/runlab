"""Error kinds the CLI maps to exit status and diagnostic wording."""


class RunLabError(Exception):
    """Base for every failure RunLab reports as a handled error."""


class DeclarationError(RunLabError):
    """A declaration is missing, malformed, or violates the protocol."""


class RealizationError(RunLabError):
    """A realization could not be produced, retrieved, or trusted.

    A locked realization that cannot be retrieved raises this rather than
    triggering a rebuild, because rebuilding silently replaces the baseline
    that earlier Runs were compared against.
    """


class ExecutionError(RunLabError):
    """The container engine could not carry out a requested operation."""
