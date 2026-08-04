# Supply Credentials

A Base or an Overlay declares credential slots by name. You materialize a private directory whose entries match those names, and RunLab mounts each entry read-only at the declared target.

## Find out what is required

```bash
uv run runlab base check bases/pi
```

```json
{"name":"pi","credentials":["deepseek"],…}
```

Check every Overlay you intend to stack as well — an Overlay that adds a tool may add a slot.

## Create the entries

The store defaults to `$XDG_CONFIG_HOME/runlab/credentials`, or `~/.config/runlab/credentials` when that is unset.

```bash
mkdir -p ~/.config/runlab/credentials
chmod 700 ~/.config/runlab/credentials

printf '%s' "$DEEPSEEK_API_KEY" > ~/.config/runlab/credentials/deepseek
chmod 600 ~/.config/runlab/credentials/deepseek
```

The file name must equal the declared slot name exactly, with no extension. A slot of kind `directory` needs a directory with mode `0700` instead.

RunLab validates every slot before accepting a Run and refuses when the root or any entry is accessible by group or others. It never creates or modifies the store.

## Point at a different store

```bash
uv run runlab run start … --credentials /private/creds
export RUNLAB_CREDENTIALS=/private/creds
```

The explicit flag wins over the environment variable, which wins over the default location.

## What ends up in the record

Only the logical name, the entry kind, and the container target:

```json
{"name": "deepseek", "kind": "file", "target": "/run/credentials/deepseek"}
```

Host paths, contents, and digests never enter a record, a log, an artifact, a workspace, or an image.

## What this does not protect against

The Base is trusted with every credential it requests. A read-only mount stops the Agent from overwriting a secret; it does nothing to stop a malicious runtime from printing or transmitting one.

Review a Base you did not write before giving it a slot, the same way you would review anything else you hand a key to.

## Credentials RunLab needs for itself

Reaching a registry or a remote store is a different concern and deliberately uses a different mechanism: the host tool chain's own authentication, such as the Docker config and the Git credential helper. Those never pass through the slot mechanism, which stays exclusively for material handed to the Agent.
