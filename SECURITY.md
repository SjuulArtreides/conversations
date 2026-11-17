# Security

Conversations can expose local terminals, files, and coding-agent sessions.
Deploy it only for trusted users while evaluating its security boundaries.

- Terminal write access and approved agent peers can cause commands to run
  locally. Treat those permissions as code-execution capabilities.
- Password registration does not verify email ownership. For shared deployments,
  prefer Google SSO and restrict the allowed hosted domain.
- Keep authentication enabled and use HTTPS outside loopback development.
- Do not publish credentials, private transcripts, or host approval material.
- Use one server replica. Live host relay state is not distributed.

The initial public snapshot has not undergone a complete independent security
audit. A publication review checks the snapshot for private material; it does
not establish that all application vulnerabilities have been fixed.

To report a vulnerability, use GitHub's private vulnerability reporting for
this repository if enabled. If it is unavailable, open an issue requesting a
private reporting channel without including exploit details or credentials.
