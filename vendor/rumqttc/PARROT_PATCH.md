# rumqttc 0.25.1 local security patch

Source: https://github.com/bytebeamio/rumqtt/tree/f1e9e8d558783f942993046679cdf3c8c3a3d36b/rumqttc

The crates.io release archive SHA-256 is
`0feff8d882bff0b2fddaf99355a10336d43dd3ed44204f85ece28cf9626ab519`.
`UPSTREAM.json` records the original file hashes and the Apache-2.0 license from
that exact upstream commit. Original hashes are preserved independently of the
patched files. Runtime/build edits change exactly `Cargo.toml` and `src/tls.rs`.
`CHANGELOG.md` additionally points its unavailable parent-directory changelog
link to that same fixed upstream commit; no historical entry is changed.

The normalized `Cargo.toml` security edit is:
`dependencies.rustls-webpki.version` changes from `0.102.8` to `0.103.13`.
The old 0.102 line has four RustSec advisories and upstream uses its public Error
type. This aligns it with the patched rustls 0.23 certificate verifier.
The PEM maintenance edit replaces optional `rustls-pemfile` with
`rustls-pki-types` (minimum 1.15.1), and updates `src/tls.rs` to its typed
`PemObject` API. Certificate parsing errors still fail, empty roots/client chains
are rejected, and the first PKCS1/PKCS8/SEC1 private key remains authoritative.
PEM errors retain the public `Error::Io` category. Provider selection, hostname
verification, ALPN and native TLS behavior are unchanged. Batch 234 records
original-file comparison and real mTLS/MQTT key-format contracts.
`Cargo.toml.orig` is retained unchanged for provenance, not used for building.

The normalized manifest also selects `../flume` by path, without a registry
fallback. MQTT v4 and v5 `EventLoop::clean` drain queued requests and accept a
waiting async publication. Upstream Flume invokes that publisher's waker under
its channel lock; a waker that calls `AsyncClient::try_publish` then deadlocks.
The local Flume fork commits pending sends and drains the queue before delivering
notifications outside the lock. The public MQTT regression exercises both
versions, checks the original and reentrant publications are retained in order,
and bounds a deadlocking child process. Real broker, TLS, ACL and recovery tests
remain separate requirements. Both direct Parrot and MQTT paths now use the same
fixed Flume source, including external path consumers.
The upstream Cargo.lock is omitted: this path dependency uses the consuming
workspace's lock. Parrot uses an explicit path dependency, not a root-only Cargo
patch, so external path consumers retain this repair. The dependency has no
registry version fallback; publishing must first resolve the upstream fix.

Parrot selects `use-rustls-no-provider` and provides the existing ring backend
through its direct rustls/tokio-rustls dependencies. TLS and certificate/name
verification remain enabled. Dependencies and removal criteria are documented
in `docs/DECISIONS_DEPENDENCIES.md`.

Remove this patch once an upstream release uses a compatible patched verifier
and a maintained PEM parser;
verify the locked graph and rerun MQTT protocol, real broker TLS/ACL/recovery,
TCP/QUIC and workspace gates. Do not exempt this copy from security advisories.
