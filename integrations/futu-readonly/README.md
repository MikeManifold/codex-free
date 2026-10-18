# Futu AppKey read access

This optional stdio MCP adapter uses an existing approved Futu AppKey. It exposes account listing, funds and positions through GET requests. It has no order, cancellation or trade-unlock tool and does not start OAuth login.

Set `FUTU_LEGACY_CREDENTIAL_DIR` to a private host directory containing `appkey_id` and `private_key.pem`. Both files must be owned by the current user with mode 0600; symlinks are rejected. Keep credentials outside the repository. Run `python3 integrations/futu-readonly/server.py`; Node.js must be on `PATH`, or set `FUTU_LEGACY_NODE` to its executable.

The signer detects Ed25519 or RSA and follows the [official authentication format](https://webapi.futunn.com/zh-cn/api/overview/getting-started): Ed25519 signs the original request bytes; RSA uses SHA256 with PKCS#1 v1.5. Unsupported key types fail without exposing key material or upstream response bodies.
