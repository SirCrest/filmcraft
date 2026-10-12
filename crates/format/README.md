# filmcraft-format

FilmCraft `.fcproj` project files (layer L3): a UTF-8 JSON envelope with an explicit schema
version, single-step migrations from every older schema, atomic saves and auto-save naming and
rotation. The schema history is in the crate docs (`src/lib.rs`); hand-made files of past schemas
live in `tests/fixtures/`.

## Nesting depth

Bins nest, and every bin level costs three JSON levels (the parent's `children` array, the
`{"Bin": …}` entry, the bin object). serde_json stops at 128 levels by default, so a project with
bins nested 42 deep saved but did not open again. Bins that deep come from the UI and from imports
(FCP7 XML allows about 60 levels, the Premiere Pro importer keeps 64).

**Decision:** `decode` reads documents nested up to `MAX_NESTING` = 512 levels, which is bins up to
`MAX_BIN_DEPTH` = 169 deep with items in the deepest bin. It first counts the nesting in a loop
(no recursion; brackets inside strings don't count) and refuses anything deeper as a damaged file.
Only then does it parse, with serde_json's own limit turned off (`unbounded_depth` feature,
`Deserializer::disable_recursion_limit`). A hostile file can't overflow the stack: it is counted
and refused before anything recursive runs.

The limit comes from the stack. Measured on Windows with `decode` at the limit, the smallest stack
that works is:

| Path | Debug | Release |
|---|---|---|
| Current schema (typed fast path) | 257 KB | 129 KB |
| Older schema (`Value`, migrate, `from_value`) | 321 KB | 257 KB |
| Worst case: objects nested to the limit parsed into a `Value` (the editor view) | 385 KB | 193 KB |

That is under half of a 1 MiB stack, the smallest `decode` runs on (the Windows main thread,
wasm). `tests/nesting.rs` runs every path at the limit on a 1 MiB thread. The model's own recursive
walks (`Bin::find_bin`, `all_items`, `parent_of`, clone, drop, serialize) cost less per level than
parsing.

Alternatives considered:

- **`serde_stacker`**, which grows the stack on demand. Rejected: its `stacker` dependency builds
  C and assembly (`psm`, through `cc`) and switches stacks with `unsafe` code. That breaks the
  pure-Rust rule, and wasm support is limited.
- **Capping bin depth in the project model and flattening deeper bins on import.** Not chosen as
  the fix: it would flatten users' bin trees from Premiere and FCP7 imports, and the format still
  needs its own bound for hostile files. The model has no depth cap today, so a bin tree deeper than
  `MAX_BIN_DEPTH` (only possible by nesting 170 bins by hand or by script) still saves but is
  refused on open. A model cap at `MAX_BIN_DEPTH` would close that gap.

Raising `MAX_NESTING` needs new stack measurements. Every bin level costs three JSON levels, so the
bin limit moves by a third as much.
