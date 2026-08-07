# The `radiroh://` URI scheme

`radicle-artifact` records **location hints** in each release COB. A location
hint is a URL, recorded under a contributor's DID, asserting where an
artifact's bytes can be fetched. HTTPS mirrors use `https://`. Peer-to-peer
fetches over [iroh-blobs](https://docs.iroh.computer/protocols/blobs) use the
Radicle-owned `radiroh://` scheme specified here.

This scheme replaces the earlier, invented `iroh://` scheme (rad issue
b93d542). That scheme was never specified, and Radicle does not own it.
Owning the namespace lets us specify what the URL means, covering both peer
discovery and the transfer protocol. It also lets us extend the scheme
without colliding with anyone else.

## Grammar

```
radiroh://<endpoint-id>
```

`<endpoint-id>` is an iroh endpoint id: a 32-byte Ed25519 public key. It is
encoded as RFC 4648 base32, lowercase, no padding (`a-z` and `2-7`). A valid
host decodes to exactly 32 bytes.

The scheme names the endpoint **identity** only. It carries no network
coordinates such as IPs or relays. Those belong to discovery, which is a
consumer-side concern. The analogy is `https://example.com`: the URL names a
host, not a DNS resolver.

## Semantics

A `radiroh://` location asserts that the bytes for a given CID can be fetched
over iroh-blobs from the named endpoint id. A consumer resolves the endpoint
id to network coordinates using its configured discovery (for example pkarr
DNS, mainline DHT, or a static map). It then performs a BLAKE3-verified
iroh-blobs fetch.

## Discovery is not in the URL or the COB

Discovery configuration is per-ecosystem infrastructure, not per-artifact
metadata. Baking a resolver into the URL, or into the COB schema, would sign
an operational detail into every artifact forever. It would also turn
resolver rotation into a signed-COB migration. Discovery therefore stays in
consumer config.

Every `radiroh://` URL carries an **implicit** dependency on the consumer's
configured discovery and relay infrastructure. The endpoint id is a public
key, and discovery must resolve it to network coordinates. This
implementation
([`crates/radicle-artifact-node/src/iroh.rs`](../crates/radicle-artifact-node/src/iroh.rs))
defaults to the public good relay and pkarr services hosted on
`radicle.garden` and `radicle.network`.

Each is overridable via environment variable. Both take a comma-separated
list, which gives redundancy. The node relays through the fastest of the given
relays and falls back to the others. It publishes to and resolves from all of
the given pkarr servers.

| Setting                 | Env var           | Default                                                                                                                              |
| ----------------------- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| Relays                  | `IROH_RELAYS`     | `eu-1.relay.iroh.radicle.garden`, `1.eu.relay.iroh.radicle.network`, `1.us.relay.iroh.radicle.network`                                |
| pkarr publish & resolve | `IROH_PKARR_URLS` | `https://dns.iroh.radicle.garden/pkarr`, `https://1.eu.dns.iroh.radicle.network/pkarr`, `https://1.us.dns.iroh.radicle.network/pkarr` |

A value that lists no entries, such as `" "` or `","`, is an error: the node
refuses to start rather than run without relays or without discovery. A relay
entry is a bare host, without a scheme, because each is served over `https://`.
A pkarr URL is a full URL and must use `https` or `http`.

Point these at your own relay and pkarr/DNS services to resolve the same
`radiroh://` URLs through different infrastructure. The URL is unchanged,
exactly as `https://example.com` resolves through whatever DNS resolver you
configure.

A future optional `?via=` query parameter is **reserved** for cold-start
hints, that is, cases where the consumer's default discovery can't reach the
endpoint. It is not specified yet.

## Compatibility

Legacy `iroh://` URLs are **not read**. A fetch ignores them, and
`EndpointId::from_url` rejects the scheme. The artifact store and these COB
locations are pre-release, so this is a hard break rather than a dual-read
transition.

Earlier versions also wrote a **bare** `radiroh://` URL with the host
omitted, deriving the endpoint id from the author's DID. Such URLs are still
read: the missing host falls back to the location author's key. The form is
deprecated and left out of the grammar above. It pins the serving endpoint to
the author's radicle key, so it can't name an endpoint backed by a separate
iroh key. Writers always emit the explicit `radiroh://<endpoint-id>` form.

To clean up `iroh://` locations from earlier versions under your DID and migrate to the new `radiroh://`, run:

```
rad-artifact reconcile --remove-orphaned-self
```
