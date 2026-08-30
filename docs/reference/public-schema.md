---
title: Public SQL Schema
description: Discover and query RunLab's bounded read-only Relations without depending on private SQLite tables.
---

# Public SQL Schema

RunLab exposes stable public Relations rather than its private SQLite schema. Discover the exact contract from the same binary that will run the query:

```bash
runlab schema list
runlab schema get runs
runlab schema get observation_types
runlab schema get observations
runlab schema get observation_retractions
runlab schema get run_deletions
```

Official and externally registered Observation Types share the same `observations` Relation. Type-specific payload values remain JSON and are selected with SQLite JSON functions; RunLab does not create a new Relation or typed columns for each Method.

`query run` accepts one bounded read-only statement. Its envelope reports whether all rows and cells were returned. A result with `complete: false` is partial evidence and must not be interpreted as a complete population.
