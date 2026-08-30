# Security Policy

## Supported versions

Security fixes are provided for the latest published RunLab release.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this repository. Do not open a public Issue containing credentials, exploit details, private Run records, or sensitive OCI content.

RunLab executes caller-selected OCI workloads with rootful Linux capabilities in its reference profile. A report should distinguish a failure to enforce RunLab's declared isolation or Secret boundary from behavior explicitly granted by the supplied OCI Runtime Configuration.
