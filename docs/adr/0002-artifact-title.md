# 2. `title` metadata as an artifact's display name

Status: accepted

## Context

An artifact has no display name of its own. Its `name` comes from the file name, which is often long and machine-oriented, for example `app-v1.2.0-x86_64-unknown-linux-gnu.tar.gz`. Readers want a short, human-friendly label such as "Linux (x86_64)".

Several clients show artifacts: the CLI, the explorer, and the desktop app. Each client can improve readability with a short label. But all clients must agree on where the label comes from and how to read it. If they do not agree, each client shows a different label for the same artifact.

## Decision

The metadata key `title` holds an artifact's display name.

- A client uses `title` only when its value is a JSON string that is not empty after trimming whitespace. The client shows the trimmed value.
- In all other cases (key missing, empty, or not a string), the client shows `name` alone.
- When a client shows `title`, it also shows `name`, in less prominent text. The file name stays visible because it identifies the file a user downloads.
- `title` is not derived from the file name. It follows the same trust rules as all other metadata keys: clients read only the last write from the artifact's author or a repository delegate. Writes from other parties have no effect on the label.

`rad-artifact list` and `show` follow this rule: the title in bold, then the file name dimmed. The key is defined as `METADATA_KEY_TITLE` in `radicle-artifact`.

## Consequences

- All clients (CLI, explorer, desktop) that follow this rule show the same label for an artifact.
- `title` is lowercase, so it complies with the camelCase rule in [ADR 1](0001-json-casing.md).
- `--json` output does not change. `title` stays in the `metadata` object, and consumers apply the rule above themselves.
- `show` also lists `title` under `metadata`, so the value appears twice. This is accepted to keep the metadata block a complete view of the stored keys.
