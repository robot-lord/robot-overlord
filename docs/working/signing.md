# Code signing

**Status:** in progress

Dev builds (built on the maintainer's Mac) are signed with a Developer ID
since 0.23.0-dev.31; see CONTRIBUTING.md, "Versions and releases".

## Done

- **Stable releases signed in CI** (same PR, #67). release.yml's macOS job
  runs in the `release` environment, imports the .p12 into a temporary
  keychain, builds with the same `sign()` as a dev build, and checks the
  binary is signed by a Developer ID with identifier
  `com.robot-lord.rlord`. A publishing run with no identity fails; a dry
  run without the secrets signs ad-hoc with a warning.
- **The web plugin** (feat/web-ui): `rlord-web` goes through the same
  `sign()`, identifier `com.robot-lord.rlord-web`, before it is hashed
  for `RLORD_WEB_SHA256`; "Verify signature" checks both binaries.
- **"Verify signature" fixed** (feat/web-ui): it read `codesign -dv`,
  which prints no `Authority=` lines, so a real Developer ID signature
  failed the check. It reads `codesign -dvv` now. main still has `-dv`
  until this branch merges (or a separate fix lands there first).

## One-time setup (maintainer)

Until this is done, real releases fail at the import step and dry runs
sign ad-hoc.

1. In Keychain Access, export the "Developer ID Application: …" identity
   (certificate plus private key) as `id.p12`, with a strong password.
2. Store it and its password as secrets of the `release` environment:

   ```bash
   base64 -i id.p12 | gh secret set MACOS_SIGN_P12 --env release -R robot-lord/robot-overlord
   gh secret set MACOS_SIGN_P12_PASSWORD --env release -R robot-lord/robot-overlord
   ```

3. In the repo's Settings → Environments, open `release` (`gh secret set
   --env` or the first workflow run creates it) and add required
   reviewers, so every run that reads the key waits for approval.
4. Delete `id.p12`.
5. Test with a manual dry run: `gh workflow run release.yml -f dry_run=true`;
   the "Verify signature" step must pass.

## Owed

- **Notarization**, only if rlord is ever downloaded through a browser
  (quarantine). It needs hardened runtime, which this setup leaves off on
  purpose so debuggers can attach.
- **Certificate renewal.** Signing picks the first valid identity by hash.
  After a renewal, set `RLORD_SIGN_IDENTITY` if more than one is valid.
