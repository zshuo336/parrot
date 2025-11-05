# Public TLS test material

These intentionally public, unencrypted test keys are never deployment credentials.
OpenSSL 4.0.3 generated one RSA-2048 and one P-256 key on 2026-10-16 (Asia/Shanghai),
then exported RSA as PKCS1/PKCS8 and EC as SEC1/PKCS8. Matching self-signed leaf
certificates use CN server.test, SAN DNS:server.test/IP:127.0.0.1,
digitalSignature, clientAuth/serverAuth, CA:FALSE and a 36500-day validity.
Exact commands are recorded in batch 234's generate-fixtures.sh; regeneration
produces different random keys, certificate serials and validity start times.
Tests exercise the actual signing keys through TLS; they do not substitute
small fake DER blobs or require OpenSSL installed at test runtime.
