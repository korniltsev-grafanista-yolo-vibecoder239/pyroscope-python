# CLAUDE.md

Repo-specific instructions for coding agents working in this repository.

## Do not reference upstream pull requests in published text

Never cite, link, or name an upstream pull request in:

- commit messages,
- pull request titles or descriptions,
- issue titles, descriptions, or comments.

This applies to any upstream project, including `DataDog/dd-trace-py`, which
`cpp/` vendors from.

Describe the change on its own terms instead: what it does, why, and how it was
verified. Where provenance genuinely matters for maintenance -- re-vendoring in
particular -- record it in the vendored tree's own docs (for example
`cpp/cpu/ddtrace_stack/VENDOR.md`) as an upstream release tag, commit, or file
path, not as a pull request.

**Why:** an upstream pull request is a moving target. It gets rebased, split,
renumbered, reworked, or closed without merging, so a reference to one ages into
a dead or misleading pointer in text that cannot be edited (commit messages) or
that people read as a statement of fact (PR and issue descriptions). Released
tags and commit SHAs are immutable and stay true.

**How to apply:** when porting code from unmerged or unreleased upstream work,
write the commit and PR description as if the feature originated here -- state
the behaviour, the design decisions, the limitations, and the verification. If
you find yourself wanting to write "see upstream PR #N so the reader understands
why", that is a sign the reasoning belongs in a code comment or in `VENDOR.md`,
where it can be kept current.
