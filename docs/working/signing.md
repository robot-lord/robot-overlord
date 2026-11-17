# Code signing

**Status:** parked

Dev builds (built on the maintainer's Mac) are signed with a Developer ID
since 0.23.0-dev.31; see CONTRIBUTING.md, "Versions and releases".

## Owed

- **Sign stable releases in CI.** release.yml builds them ad-hoc. Moving
  between a stable build and a dev build changes the binary's identity, so
  macOS asks for folder access again. Needs the certificate and key as an
  Actions secret (a .p12 and its password), a temporary keychain in the
  macOS job, and `RLORD_SIGN_IDENTITY` set there.
- **Notarization**, only if rlord is ever downloaded through a browser
  (quarantine). It needs hardened runtime, which this setup leaves off on
  purpose so debuggers can attach.
- **Certificate renewal.** Signing picks the first valid identity by hash.
  After a renewal, set `RLORD_SIGN_IDENTITY` if more than one is valid.
