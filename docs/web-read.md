# Reading artifact COBs on the web

This note explores how a browser could **read** an artifact Release without a
local radicle installation: no node, no git storage, no Rust toolchain. It
covers what reading actually involves, three architectures for getting there,
and the trade-offs between them. Writing (creating and signing COB ops from the
browser) is a harder, separate problem and is out of scope here.

## What "reading a COB" involves

A Release is not a document you fetch; it is the folded result of replaying a DAG
of signed operations. Reading it end to end has four steps:

1. **Obtain the op DAG** — the signed COB entries stored as git objects under
   `refs/cobs/org.radworks.artifact/<id>`.
2. **Order them** — topologically sort the DAG into a causal sequence.
3. **Verify and filter** — check each entry's Ed25519 signature, and apply the
   trust model (by default only delegate- or local-authored ops count).
4. **Fold** — run the reducer (`from_root` + `op`) over the ordered ops to
   produce the `Release` state.

Step 4, and the signature math in step 3, are **pure**: no I/O, no git. That is
the part that can move to the browser as a WASM reducer. Steps 1 and 2, and the
delegate-set lookup in step 3, depend on git storage and repo identity, which is
exactly what the `radicle` crate (and its libgit2 dependency) provides and what
cannot run unaided in a browser.

The design space is therefore a single question: **where does each of those four
steps run, and how much does the browser trust the thing that runs the rest?**

## Architectures

### A. Node-side rendering

The radicle node computes the Release state server-side and serves it as JSON
over HTTP. The browser only renders.

```
radicle node ──(fold + verify)──> JSON ──HTTP──> browser renders
```

- No WASM needed. The browser is a thin view over an API.
- Smallest amount of code and the least data on the wire (one folded object, not
  the whole op log).
- **Trust:** the browser believes whatever the node computed. A malicious or
  buggy node can hide a redaction, omit an artifact, or misreport an attestation
  and the browser cannot tell. Signatures were checked by the node, not by you.
- Best when the node is your own or otherwise trusted, and you want the simplest
  possible path to a working web view.

### B. Hybrid: node serves the DAG, browser folds in WASM

The node serves the raw, signed op DAG. The browser runs the WASM reducer to
order, verify, and fold locally.

```
radicle node ──(raw signed entries)──HTTP──> browser ──(WASM: verify + fold)──> state
```

- **Trust-minimized.** The browser verifies every signature itself and computes
  the canonical state itself; the node becomes an untrusted blob transport. A
  tampered or partial response fails verification rather than silently lying.
- One canonical reducer (the same WASM) produces the same state everywhere, so a
  web view and the CLI cannot diverge.
- **Cost:** more bytes on the wire (the full op log, not the folded result), and
  more client code: the WASM reducer plus a host shim for ordering and for
  obtaining the **delegate set** (the trust filter still needs repo identity,
  which the node must supply as input the browser can independently check, e.g.
  the signed identity document).
- The honest sweet spot for a trust-respecting web reader.

### C. Full peer-to-peer in the browser

The browser speaks the radicle/git protocol directly, fetches refs itself, and
does everything locally with a wasm git implementation.

```
browser ──(git/radicle protocol via ws gateway)──> peers
        └── wasm-git + WASM reducer: fetch, order, verify, fold
```

- Maximal decentralization: no privileged node in the path.
- **Heaviest by far.** Browsers have no raw sockets, so peer connections need a
  websocket or WebTransport gateway. You also ship a wasm git stack and a much
  larger surface to maintain.
- Justified only when removing any trusted server is a hard requirement.

## Trade-offs at a glance

| | A. Node renders | B. Hybrid (WASM fold) | C. Full P2P |
| --- | --- | --- | --- |
| WASM reducer needed | no | yes | yes |
| wasm git stack | no | no | yes |
| Trust in the node | full | transport only | none |
| Verifies signatures client-side | no | yes | yes |
| Data on the wire | folded state | full op log | full op log |
| Client complexity | low | medium | high |
| Decentralization | low | medium | high |

## Recommendation

Start with **A** to get a working web view quickly, then move the fold and
verification into the browser (**B**) once the radicle-free reducer crate exists
and you want the web view to be trust-minimized rather than node-dependent. **C**
is a separate, much larger effort and only pays off when a trusted gateway is
itself unacceptable.

Two pieces gate B and C regardless of which you pick:

- **Extracting a `radicle`-free reducer** so the fold can compile to
  `wasm32`. Today the state machine is coupled to `radicle::cob`, so the reducer
  cannot leave the native stack.
- **Surfacing the delegate set as verifiable input.** Trust filtering is not
  pure; the browser must receive the repo identity (signed) and check it, rather
  than trusting the node's filtering.

Reading is the tractable half of "COBs on the web." It needs no key custody and
no write path, so it is the right place to start.
