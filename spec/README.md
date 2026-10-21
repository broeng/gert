# Gerrit OpenAPI spec

`rest-api-openapi.json` is vendored unmodified (byte for byte).

- Source: https://github.com/davido/gerrit-sdk-rust, file `rest-api-openapi.json`
- Tag: `v3.15.0-SNAPSHOT`
- Commit: `b6376aeb3b3abcd1240d601ed6a93d72b7a1ef37`
- Gerrit version (`info.version`): `3.15.0-SNAPSHOT`
- License (`info.license`): Apache-2.0, https://www.apache.org/licenses/LICENSE-2.0
- The spec is emitted by Gerrit's OpenAPI emitter
  (Gerrit issue 40011133), not written by hand.

We use it as the reference for field names and types, as the input for
`cargo xtask gen-fixtures`, and `gerrit-api` has a test that checks every
wire struct field exists in the matching `components.schemas` entry. We do
not depend on the SDK crate itself.

## Updating

1. Fetch the new `rest-api-openapi.json` (from the repository above at the
   wanted tag, or generate it from a Gerrit checkout with the OpenAPI emitter).
2. Copy it here unmodified and update the tag, commit and version above.
3. Run `cargo xtask gen-fixtures` and `cargo test --workspace`, and review the
   fixture diff.
