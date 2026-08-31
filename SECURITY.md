# Security policy

## Reporting a vulnerability

Please do not open a public issue for a vulnerability that could expose credentials, account data, native-client state, or a way to bypass a provider's safeguards.

Use GitHub's **Report a vulnerability** button on this repository's Security page to send a private report. Include:

- a clear description of the impact;
- minimal reproduction steps that use fake credentials and redacted paths;
- affected subswap version, operating system, and native client version;
- any mitigation you already tested.

Do not attach access tokens, refresh tokens, API keys, complete login files, real account identifiers, or billing screenshots.

## Scope

Security reports are especially useful for accidental credential disclosure, unsafe filesystem permissions, incomplete rollback, unsafe native-client coordination, installer integrity, and dependency or release-chain compromise.

subswap will acknowledge a valid report privately, investigate it, and coordinate a fix before public disclosure when practical. Please give maintainers reasonable time to respond before publishing details.

## Supported versions

Security fixes are made for the latest published release. Users should update to the latest version before reporting an issue unless doing so would make the problem impossible to reproduce.
