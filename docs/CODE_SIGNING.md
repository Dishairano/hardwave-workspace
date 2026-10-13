# Signing the Windows build

The Workspace app is signed with Azure Trusted Signing, the same account and the same script as
the DAW and the Suite (`hardwave-daw/docs/CODE_SIGNING.md` has the full story).

- `scripts/sign-windows.ps1`: the signing script. Tauri runs it on every file it bundles
  (`bundle.windows.signCommand` in `src-tauri/tauri.conf.json`).
- Repository variables, set: `AZURE_TRUSTED_SIGNING_ENDPOINT`, `AZURE_TRUSTED_SIGNING_ACCOUNT`,
  `AZURE_TRUSTED_SIGNING_PROFILE`.
- Repository secrets, to set once from the same values the DAW repository has:
  `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`.

On a release tag (`v*`) a missing setting or a signature that does not check out stops the build.
