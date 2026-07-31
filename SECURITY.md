# Security policy

## Supported versions

Security fixes are applied to the latest published release. Older releases may
be asked to upgrade before a fix is evaluated.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting for this repository:

1. Open the repository's **Security** tab.
2. Choose **Report a vulnerability**.
3. Include affected versions, platform, reproduction steps, impact and any
   proposed mitigation.

Do not open a public issue for an unpatched vulnerability and do not include
real credentials, private prompts or customer source code in a report. Use
minimal synthetic examples whenever possible.

If private vulnerability reporting is temporarily unavailable, open a public
issue containing no vulnerability details and ask the maintainer for a private
contact channel.

## Scope notes

AI Bridge coordinates local Claude Code, Codex CLI, Git and shell processes.
Reports about command construction, path handling, credential leakage, review
gate bypass, unsafe update behavior or untrusted repository content crossing a
trust boundary are especially useful. Vulnerabilities in third-party services
or CLIs should also be reported to their respective maintainers.
