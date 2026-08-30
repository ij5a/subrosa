# Security policy

## Supported versions

Only the latest release gets security fixes. Update before reporting a problem. Use the plugin (`/plugin`), Homebrew (`brew upgrade ij5a/tap/subrosa`), or cargo (`cargo install --git https://github.com/ij5a/subrosa --locked --force`).

## Reporting a vulnerability

Report security problems privately, not in a public issue.

Use GitHub's private reporting: open the repository's **Security** tab and choose **Report a vulnerability**. That sends the report straight to the maintainer and keeps it private until a fix is out.

This small project responds on a best-effort basis. You will get a reply confirming receipt. The fix is then evaluated, tested, and shipped in a new release. There is no fixed timeline. We take security reports seriously.

## What redaction does and does not do

subrosa masks these high-value secret shapes before storing transcript text:

- private-key blocks (`-----BEGIN … PRIVATE KEY-----`)
- AWS access keys (starting with `AKIA` or `ASIA`)
- `Bearer` tokens
- labeled secrets like `password=…` or `token: …`

This is best-effort pattern matching, **not** full cleanup. A secret outside these shapes, such as a GitHub `ghp_…` token, an OpenAI `sk-…` key, or a bare JWT, is stored as written. Treat your local archive as sensitive. It can contain anything from your Claude Code transcripts.

A few more things worth knowing:

- **Your original transcripts stay in cleartext.** Claude Code writes them under `~/.claude/projects`. subrosa never edits them. Redaction covers only subrosa's archive copy. Full-disk encryption (FileVault, LUKS) protects data at rest.
- **File permissions are access control, not encryption.** Unix uses owner-only permissions (`0600` / `0700`) for the database and its folder. Windows uses default ACLs.
- **Recall re-injects your stored text.** A strong match puts up to three short snippets into the model's context. Leaked archive data can resurface there.
- **One snapshot can leave the machine by choice.** The optional backup mirror is off by default and uses `subrosa setup`. An iCloud or Dropbox folder lets its sync client upload the snapshot.

The binary itself makes zero network calls. See [Proof](docs/faq.md#proof) in the FAQ for checks of network behavior, model pinning, token limits, and dependencies.
