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
| `APPLE_API_KEY_ID` | App Store Connect **team** API-key identifier for notarization. |
| `APPLE_API_ISSUER_ID` | Issuer identifier for that team key. |
| `APPLE_API_PRIVATE_KEY` | Contents of the matching `.p8` team-key file. |

The certificate must be valid for Developer ID signing. Use an App Store Connect
**team** API key for notarization: [Apple says individual API keys cannot use
`notarytool`](https://developer.apple.com/documentation/appstoreconnectapi/creating-api-keys-for-app-store-connect-api).
Keep all six values in GitHub Actions secrets;
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
   ./tests/scenarios/release_artifact_checks.sh
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
   both archives, and attaches `SHA256SUMS` to the GitHub Release. On a Mac,
   download all three assets and run `scripts/verify-macos-release.sh` against
   them, passing your Apple Team ID from the Apple Developer account as the
   third argument. This checks the archive hashes and exact contents, both
   binaries' architecture and Developer ID signatures, the expected signing
   team, secure timestamps, and Gatekeeper acceptance. A passing workflow
   alone is not the final release check.
7. Clone the tap beside this repository. Generate the Homebrew formula from
   the **published** release with the checked download helper, then review it
   and open a PR against the tap. The helper verifies both archives as in step
   6 and never accepts hand-copied hashes. Stage the candidate separately so
   a failed check cannot truncate an existing tap formula. From this
   repository's root:

   ```sh
   candidate_dir="$(mktemp -d)"
   ./scripts/prepare-homebrew-formula.sh 0.1.0 YOURTEAMID \
     > "$candidate_dir/reccursive.rb" &&
     brew audit --strict "$candidate_dir/reccursive.rb" &&
     brew install "$candidate_dir/reccursive.rb" &&
     mkdir -p ../homebrew-tap/Formula &&
     cp "$candidate_dir/reccursive.rb" ../homebrew-tap/Formula/reccursive.rb
   ```

8. After the formula PR is merged, on a clean macOS account install from the
   release archive and from the tap. Run `reccursive doctor`, enroll a
   throwaway repository, install the service, then publish one harmless
   scheduled change. Record the results before announcing public availability.

Only after step 8 should the project advertise `brew install
Aaryan1524/tap/reccursive` as a supported installation path.

## Recovery

If a tag workflow fails before the GitHub Release is created, fix the cause on
`main`, delete the failed tag locally and remotely, and create a new tag only
after review. If the GitHub Release has already been published, do not replace
its assets: release a new patch version so users can identify exactly what they
installed.
