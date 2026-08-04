# ADR-0008: Scoped bearer tokens, and refusing to serve openly on a public interface

- **Status:** Accepted
- **Date:** 2026-08-04
- **Owners:** Server, Security
- **Related:** [ADR-0006](0006-server-resource-limits.md), [ADR-0007](0007-sqlite-durable-storage.md)

## Context

ADR-0006 bounded what an unauthenticated peer can consume and said plainly what
it did not do: authentication "remains the top security item: the server still
admits every connection." ADR-0007 then sharpened the cost of that gap — an
unauthenticated writer now leaves *durable* rather than transient state.

The standalone server had one authority function, `authorize()`, which returned
`true`. It was one function on purpose, so the gap stayed obvious rather than
diffusing through the request path.

Two constraints shape the answer.

The engine has no opinion about identity and must keep none: a host embedding
k-board replaces this binary with its own membership check and never reaches any
of this. Whatever is built here has to be replaceable in one place.

And the server binds loopback. That reframes the problem. An unauthenticated
board has never been reachable from another machine, so the dangerous
configuration is not "no secret" — it is **"no secret *and* a public bind."**

## Decision

**The dangerous combination is refused at startup, not warned about.**
`KBOARD_BIND` exists so the refusal has something to refuse, and the check runs
before a database is opened or a port is bound.

| Secret | Bind | Result |
|---|---|---|
| unset | loopback | runs open — development convenience |
| unset | anything else | **refuses to start** |
| set | any | tokens enforced on every handshake |

Forcing a secret on someone running the quickstart on their own laptop buys no
safety and costs the first five minutes of everyone's experience. Warning about
an unauthenticated writable store on a network buys nothing at all, because a
warning at boot is read once and never again.

**A token authorises exactly one scope.** This is the property that matters:
it makes the scope in the URL untrusted input rather than an authorisation
decision. A grant for `tenant-a:board` cannot open `tenant-b:board`.

**Tokens are self-contained**, so verification needs no lookup, no shared state
between processes, and no database round trip on a handshake:

```
base64url(payload) "." base64url(hmac-sha256(secret, base64url(payload)))
```

where the payload names the scope and an expiry.

**Primitives rather than a JWT library.** The format is forty lines, fully
specified in one file. A library brings algorithm negotiation and the `alg:
none` family of mistakes to a problem that has exactly one algorithm.

Three details are load-bearing and easy to undo by accident:

- The signature is verified **before the payload is parsed.** An attacker must
  not be able to reach the parser with an unsigned payload.
- The comparison is **constant-time**, so timing does not reveal how much of a
  forged signature was right.
- Refusals are **logged with a reason and returned without one.** A client that
  learns *why* its token failed learns something about the secret.

**The token travels as a WebSocket subprotocol**, not a query parameter.
Browsers cannot set headers on a handshake, and a URL ends up in server logs,
browser history, and referrers.

**Minting lives in the server binary.** `kboard-server --token <scope>` prints a
grant and exits, so issuing one needs no second tool and no second copy of the
token format. Minting without a secret fails rather than handing back a token an
open server would ignore.

## Alternatives considered

| Alternative | Advantages | Disadvantages | Rejection reason |
|---|---|---|---|
| A JWT library | Standard; interoperable; someone else's bugs are found already | Algorithm negotiation, `alg: none`, key-confusion, and a dependency tree, for a format with one algorithm and one issuer | Surface area far exceeding the problem |
| Require a secret always | One configuration; no branch to get wrong | Breaks `cargo run` and every quickstart, buying no safety on a loopback bind | Punishes the safe case to discipline the unsafe one |
| Warn on an open public bind | Nothing stops working | A boot warning is read once; the process still serves an unauthenticated writable store | Not a configuration to warn about |
| Token in a query parameter | Trivial for any client | Lands in server logs, browser history, and referrer headers | Leaks the credential by construction |
| Session cookies | Browser-managed; revocable | Needs server-side session state, a login endpoint, and CSRF handling; useless to a native host | Wrong shape for an embeddable engine |
| Server-side session table | Revocation is immediate | Reintroduces shared state between processes and a lookup on every handshake | Self-contained tokens are the reason this needs no storage |
| Per-user identity rather than per-scope grants | Richer authorisation | Requires a user model the standalone server does not have and the host already has | The host owns identity; this owns admission |
| mTLS | Strong; no bearer secret | Certificate distribution to browsers is not a thing anyone will do | Unusable for the standalone product's audience |

## Consequences

### Positive

- The scope in a URL is no longer an authorisation decision.
- Verification is O(1) with no I/O, so authentication does not touch the room
  lock or the store.
- The whole mechanism is one module. A host that replaces it deletes one file's
  worth of behaviour and nothing else changes.
- The quickstart still works with no configuration.

### Negative and accepted trade-offs

- **No revocation before expiry.** A leaked token is valid until it expires
  (12 hours by default). Self-contained tokens buy statelessness by giving this
  up, and a revocation list would put back the lookup the design removed.
- **One shared secret.** Rotating it invalidates every outstanding grant at
  once. There is no key id in the payload, so a rolling rotation is not
  possible without a format change.
- **Secret distribution is unsolved.** `KBOARD_SECRET` is an environment
  variable; how it gets there is the operator's problem.
- The subprotocol trick is slightly unusual and will look like a mistake to a
  reader who does not know browsers cannot set handshake headers. The code says
  why.

### Operational consequences

`KBOARD_SECRET` enables enforcement; `KBOARD_BIND` selects the interface. The
boot banner states which mode is active rather than leaving it to be inferred.
A server that refuses to start prints the reason and the two ways to fix it.

### Security consequences

This closes the gap ADR-0006 named. What it does *not* close: there is no
transport encryption, so a token on the wire is readable by anyone in path
unless a reverse proxy terminates TLS. On a loopback bind that does not matter;
on any other bind it very much does, and the refusal rule means a public bind
now at least implies somebody configured a secret deliberately.

## Validation

- Nine unit tests: round trip, cross-scope refusal, wrong-secret refusal,
  expiry, a tampered payload carrying a valid signature, malformed inputs that
  must not panic, open-authority behaviour, and the bind rule for both open and
  enforcing servers.
- `scripts/auth-check.mjs` in CI proves the claim against a **running process**,
  which is the only thing that demonstrates it. The unit tests show a signature
  verifies; they cannot show the server refuses. The check performs the
  WebSocket handshake by hand, because a WebSocket client reports a rejected
  upgrade as an opaque error event that cannot distinguish "refused" from "not
  running" — the raw status can.

## Revisit triggers

- Revocation before expiry becomes necessary, which reopens statelessness.
- More than one issuer or a rolling key rotation, which needs a key id in the
  payload and therefore a format version.
- The standalone server grows a user model, at which point per-scope grants may
  be the wrong granularity.
- TLS termination becomes part of the product rather than the operator's job.
