# Security policy

Handoff runs commands on someone's PC for a person they've sent a link to, so I take security reports seriously.

## Reporting

Please don't open a public issue. Use [a private security advisory](https://github.com/RealWhyKnot/Handoff/security/advisories/new) so we can work out a fix before anything is published.

I try to reply within 7 days. There's no bug bounty.

## In scope

- Running a command that should have asked the host first without asking, or getting past `--consent deny`.
- Reaching a session without its link, or guessing or leaking a session token.
- A command doing more than its documentation says, such as reading or writing outside the path it was given.
- `handoff update` accepting a file that doesn't match the published SHA-256.

## Out of scope

- Anything the helper does with a link the host chose to share. The link is meant to give them access.
- Attacks that need admin access to the host PC already.
