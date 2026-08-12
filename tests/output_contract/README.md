# Output contract tests

This suite tests the four user-facing tools as black boxes:

```text
Tree ─┐
Twig ─┼─ deterministic fixture ── semantic parser/oracle
Unearth┤
Copy ─┘                         └─ exact rendering goldens
```

The fixture is created in a temporary directory with fixed timestamps, stable
names, symlinks, hardlinks, hidden entries, executable files and nested
directories. Expected values come from Python's `os.scandir`, `os.lstat` and
`os.walk`; no tool implementation is imported by the oracle.

Semantic tests compare paths, hierarchy, types, classifications, counts,
ordering, relation categories and planned bytes. Small presentation assertions
also protect branch glyphs, columns, ANSI colour and OSC 8 hyperlink framing.
ANSI and OSC sequences are tokenized separately so a hyperlink regression cannot
be hidden by a broad "strip all escape codes" regular expression.

Copy's preview matrix treats identity as type + size + modification time and
tests collision policy separately. It compares the parsed preview tree and both
summary tables, checks that `--preview` never mutates the fixture, and performs
a representative preview-to-real-operation parity check. Numeric flags use
boundary classes (`0`, `1`, exact limit and above); arbitrary paths and sizes are
not duplicated because they do not create new command semantics. The generated
Copy cases cover every valid combination of the finite mode, target, merge,
overwrite, display and preview families, while a separate invalid matrix checks
that contradictory flags fail before planning. Fixed-seed mixed fixtures add
repeatability coverage for deeper trees.

Run the suite after building the workspace:

```sh
python3 -m unittest discover -s tests/output_contract -v
```

Set `FSX_TREE_BIN`, `FSX_TWIG_BIN`, `FSX_UNEARTH_BIN` or `FSX_COPY_BIN` to test
specific binaries. CI runs this suite in addition to each tool's existing unit
and integration tests.
