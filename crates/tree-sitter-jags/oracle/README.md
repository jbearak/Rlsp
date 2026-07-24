# JAGS black-box syntax oracle

This directory records independently authored probes of the public JAGS 4.3.2
command-line interface. The grammar is derived from these observed inputs and
outputs, not from JAGS source code or prose documentation.

`model in "PATH"` is the syntax boundary. The controls establish that unknown
functions and distributions pass that command and fail only after a subsequent
`compile`, while malformed delimiters and expressions fail during `model in`.
The oracle therefore treats the parse-error heading as syntax rejection and
does not interpret compilation errors as syntax errors.

Run the matrix on macOS/Homebrew with:

```sh
python3 crates/tree-sitter-jags/oracle/jags_oracle.py
```

The manifest pins the official release URL and SHA-256 reported by Homebrew.
The release archive was downloaded only to verify its digest; its contents were
not inspected. The installed terminal executable digest is separate from the
shell-wrapper digest.

The matrix targets JAGS 4.3.2 only. It is not a compatibility claim for
OpenBUGS, WinBUGS, MultiBUGS, NIMBLE, or other BUGS-family implementations.
