# Security policy

## Reporting a vulnerability

Please report a security problem **privately**, not in a public issue or on
Discord. Use GitHub's private vulnerability reporting: on the repository's
**Security** tab choose **Report a vulnerability**
(<https://github.com/RoxBuilds/Rox-box/security/advisories/new>). Only the
maintainers can read it.

Tell us, in your own words and in platform terms:

- the Rox-box version (`victory.exe --version`) and your Windows build;
- what an attacker needs (a crafted file, a network peer, local access) and
  what they get;
- the smallest steps that show it, using input you made yourself.

Do not send title files, keys, logs, screenshots, video or crash dumps, and do
not name a title: the clean-room rules in [CONTRIBUTING.md](CONTRIBUTING.md)
apply to security reports too. A synthetic test file that your own tool
generated is ideal.

This is a small volunteer project: we aim to acknowledge a report within a
week, but cannot promise a fix date. We will credit you in the release notes if
you want that.

## What this layer is, and is not

Rox-box **loads and runs a title's own machine code** inside its process, by
design. That code is untrusted, and Rox-box is **not a sandbox**: it makes no
attempt to confine a title from your files, your network or your Windows
account. A title runs with the permissions of the user who started it. Only
run title files you obtained legally and trust, and do not run Rox-box as an
administrator.

So, honestly:

- "A title can read, write or delete files, or use the network, as my user"
  is how this layer works, not a vulnerability. You are welcome to describe
  what you found, and we may add warnings or documentation, but we will not
  treat it as a fixable security bug.
- Things that **are** in scope, because they go beyond running the title as
  you: a flaw in how Rox-box itself handles input (a malformed file or
  setting that corrupts memory in our own code), a way for one title's
  data to reach another title's data or the cache and save folders in ways
  we did not intend, an unsafe default in the launcher or the release
  package (for example the launcher writing outside the folders it documents),
  or a problem in how a release is built or published.
- Also in scope: a way that lets someone who is **not** the player (a web
  page, a network peer, another local user) make Rox-box run code or write
  files.

The title code runs in the same process as the layer, so a memory-safety bug in
the layer that a title can trigger is worth reporting even if the title is
untrusted anyway: it is a bug we want to fix.

## Supported versions

Only the latest release is supported. Fixes go into a new release; older
releases are not patched.
