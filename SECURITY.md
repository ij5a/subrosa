# Security policy

## Supported versions

Only the latest release gets security fixes. Update before reporting a problem with the plugin (`/plugin`), Homebrew (`brew upgrade ij5a/tap/subrosa`), or cargo (`cargo install --git https://github.com/ij5a/subrosa --locked --force`).

## Reporting a vulnerability

Report security problems privately, not in a public issue.

Open the repository's **Security** tab and choose **Report a vulnerability**. The report goes to the maintainer and stays private until a fix ships.

This small project responds on a best-effort basis. You get a receipt reply. The fix is evaluated, tested, and shipped in a new release. There is no fixed timeline.

## What redaction does and does not do

subrosa masks these high-value secret shapes before storing transcript text:

- private-key blocks (`-----BEGIN … PRIVATE KEY-----`)
- AWS access keys (starting with `AKIA` or `ASIA`)
- `Bearer` tokens
- labeled secrets like `password=…` or `token: …`

This is best-effort pattern matching, **not** full cleanup. Secrets outside these shapes, such as a GitHub `ghp_…` token, an `sk-…` key, or a bare JWT, stay as written. Treat the local archive as sensitive; it can contain anything from your transcripts.

A few more things worth knowing:

- **Original transcripts stay in cleartext.** They are under `~/.claude/projects` and subrosa never edits them. Redaction covers only subrosa's archive copy. Full-disk encryption (FileVault, LUKS) protects data at rest.
- **File permissions are access control, not encryption.** Unix uses owner-only permissions (`0600` / `0700`) for the database and its folder. Windows uses default ACLs.
- **Recall re-injects stored text.** A strong match puts up to 3 short snippets into the model's context. Leaked archive data can resurface there.
- **One snapshot can leave the machine by choice.** The optional backup mirror is off by default and uses `subrosa setup`. An iCloud or Dropbox folder lets its sync client upload the snapshot.

The binary makes zero network calls. See [Proof](docs/faq.md#proof) in the FAQ for network, model, token-limit, and dependency checks.
