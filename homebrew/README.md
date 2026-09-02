# Homebrew

`greeg.rb.in` is the formula template; the release workflow fills in the
version and sha256 of each tarball and attaches `greeg.rb` to the GitHub
release. To publish a tap, copy that file to
`<owner>/homebrew-greeg/Formula/greeg.rb`; users then run

    brew install <owner>/greeg/greeg

Without the tap: `cargo install --git https://github.com/thiagodmont/greeg greeg`
(or `cargo install greeg` once published to crates.io), or download the
tarball for your platform from the release page.
