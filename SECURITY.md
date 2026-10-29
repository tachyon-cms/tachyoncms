# Security Policy

## Supported versions

TachyonCMS is pre-alpha. Until 1.0, security fixes land on `main` and ship in the next 0.x release. Older 0.x releases don't get backported fixes, so upgrade to the latest.

| Version | Supported |
|---|---|
| Latest 0.x release | Yes |
| `main` | Yes |
| Older 0.x releases | No |

How TachyonCMS protects your content, and what it doesn't protect yet, is written up in the [security model](https://docs.tachyoncms.com/security-model).

## Reporting a vulnerability

Please don't report security issues in public issues, discussions or pull requests.

Report them privately through [GitHub's private vulnerability reporting](https://github.com/tachyon-cms/tachyoncms/security/advisories/new). If you can't use GitHub, email [security@tachyoncms.com](mailto:security@tachyoncms.com).

It helps to include:

- what an attacker could do, and what they need first (an account, network access, a particular config)
- steps or a request that reproduces it
- the commit or version you tested

## What happens next

- We'll acknowledge your report within 3 working days.
- We'll confirm whether it's a vulnerability and agree a fix timeline with you, usually within 14 days.
- We'll credit you in the advisory unless you'd rather stay anonymous.

Please give us a reasonable chance to ship a fix before you disclose publicly. We'll keep you updated the whole way.
