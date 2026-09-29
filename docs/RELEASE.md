# Releasing ReccursiveCommits

ReccursiveCommits is distributed as two native macOS executables:
`reccursive` and `reccursive-daemon`. A release is a version tag, two signed
and notarized archives (Intel and Apple silicon), their SHA-256 checksums, and
a GitHub Release. A release is not complete merely because the binaries build.

## One-time GitHub configuration

The tag workflow refuses to publish unsigned artifacts. Configure these
repository secrets before the first release:

| Secret | Purpose |
| --- | --- |
| `APPLE_SIGNING_CERTIFICATE_P12_BASE64` | Base64-encoded Developer ID Application `.p12` certificate. |
| `APPLE_SIGNING_CERTIFICATE_PASSWORD` | Password protecting that `.p12`. |
| `APPLE_SIGNING_IDENTITY` | The full Developer ID Application signing identity. |
| `APPLE_API_KEY_ID` | App Store Connect API-key identifier for notarization. |
| `APPLE_API_ISSUER_ID` | App Store Connect API-key issuer identifier. |
| `APPLE_API_PRIVATE_KEY` | Contents of the matching `.p8` API-key file. |

The certificate must be valid for Developer ID signing and the API key must be
authorized for notarization. Keep all six values in GitHub Actions secrets;
never put them in this repository or pass them on a command line.

Create a separate `Aaryan1524/homebrew-tap` repository before promising the
Homebrew command publicly. It contains the generated `Formula/reccursive.rb`.

## Release checklist

1. Start from a clean, reviewed `main` with green CI.
2. Run the complete local check suite:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-targets --locked
   cargo build --workspace --release --locked
   ./tests/scenarios/phase15_multi_batch_checkout.sh
   ```

3. On a real Mac, run the manual launchd/sleep acceptance test described in
   the README from a clean macOS account. The script refuses to replace an
   existing Reccursive launch agent. Confirm `service install`, reboot/login
   persistence, and `service uninstall` on a non-development state directory.
4. Run a live GitHub pull-request scheduling test against a disposable
   repository, inspect the resulting pull request, and merge it manually. Use
   a revocable least-privilege token.
5. Update `CHANGELOG.md`, ensure the workspace version is the intended SemVer
   version, then create and push an annotated tag:

   ```sh
   git tag -a v0.1.0 -m "ReccursiveCommits 0.1.0"
   git push origin v0.1.0
   ```

6. Wait for the **Release** workflow. It tests, signs, notarizes, publishes
   both archives, and attaches `SHA256SUMS` to the GitHub Release. Download an
   archive and verify both its checksum and Gatekeeper assessment before
   announcing it.
7. Generate and review the Homebrew formula using the two hashes in
   `SHA256SUMS`, then commit it to the tap repository:

   ```sh
   ./scripts/render-homebrew-formula.sh 0.1.0 X86_64_SHA256 AARCH64_SHA256 \
     > Formula/reccursive.rb
   brew audit --strict Formula/reccursive.rb
   brew install ./Formula/reccursive.rb
   ```

8. On a clean macOS account, install from the release archive and from the
   tap. Run `reccursive doctor`, enroll a throwaway repository, install the
   service, then publish one harmless scheduled change.

Only after step 8 should the project advertise `brew install
Aaryan1524/tap/reccursive` as a supported installation path.

## Recovery

If a tag workflow fails before the GitHub Release is created, fix the cause on
`main`, delete the failed tag locally and remotely, and create a new tag only
after review. If the GitHub Release has already been published, do not replace
its assets: release a new patch version so users can identify exactly what they
installed.
