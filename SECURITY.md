# Security policy

## Supported versions

Only the latest release gets security fixes. If you are reporting a problem, please update to the newest version first — through the plugin (`/plugin`), Homebrew (`brew upgrade ij5a/tap/subrosa`), or cargo (`cargo install --git https://github.com/ij5a/subrosa --locked --force`).

## Reporting a vulnerability

Please report security problems privately, not in a public issue.

Use GitHub's private reporting: open the repository's **Security** tab and choose **Report a vulnerability**. That sends the report straight to the maintainer and keeps it private until a fix is out.

This is a small project, so the response is best-effort. You will get a reply confirming the report was received; after that the fix is evaluated, tested, and shipped in a new release. There is no fixed timeline, but security reports are taken seriously.

## What redaction does and does not do

subrosa masks a few high-value secret shapes before it stores transcript text:

- private-key blocks (`-----BEGIN … PRIVATE KEY-----`)
- AWS access keys (starting with `AKIA` or `ASIA`)
- `Bearer` tokens
- labeled secrets like `password=…` or `token: …`

This is best-effort pattern matching, **not** a full clean-up. A secret that does not match one of those shapes — a GitHub `ghp_…` token, an OpenAI `sk-…` key, a bare JWT — is stored as written. So treat your local archive as sensitive: it can hold whatever was in your Claude Code transcripts.

A few more things worth knowing:

- **Your original transcripts stay in cleartext.** Claude Code writes them under `~/.claude/projects` and subrosa never edits those — redaction only covers subrosa's own archive copy. Full-disk encryption (FileVault, LUKS) is the real protection for data at rest.
- **File permissions are access control, not encryption.** The database and its folder are owner-only (`0600` / `0700`) on Unix; on Windows they fall back to default ACLs.
- **Recall re-injects your own stored text.** On a strong match it puts up to three short snippets back into the model's context, so anything that did leak into the archive can resurface there.
- **One snapshot can leave the machine, by your choice.** The optional backup mirror (off by default, set with `subrosa setup`) copies a snapshot to a folder you pick; point it at an iCloud or Dropbox folder and your sync client uploads it.

The binary itself makes zero network calls. See the [privacy model](README.md#privacy-model) in the README for the full picture and the commands to verify each claim.
