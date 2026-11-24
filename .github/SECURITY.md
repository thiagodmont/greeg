# Security policy

## Supported versions

Only the latest release gets security fixes. Upgrade before reporting:
`brew upgrade greeg`, or download the newest archive from
[Releases](https://github.com/thiagodmont/greeg/releases).

## Reporting a vulnerability

Please don't open a public issue for a security problem. Report it privately
through GitHub: the **Security** tab → **Report a vulnerability**.

Include the greeg version (`greeg --version`), your OS, the command you ran,
and what an attacker gains. A minimal repository or file that reproduces it
helps most.

## Scope

greeg reads files on your machine, writes an index under its cache directory,
and can install a hook that rewrites your agent's `rg` and `grep` commands.
Reports about these are in scope, for example:

- a rewritten command that runs something other than the search it replaced
- reading or writing outside the searched tree, the index directory or the
  config directory
- a crafted file or index that makes greeg crash, hang or run code
