# jq-compatible structured queries

Author: [Adam Hupp](mailto:adamh@openai.com)

**Summary:** Add a `--jq` output mode to commands such as `jj log`. The mode
evaluates a jq-compatible filter against a stable, semantic object for each
result and writes the filter's results as JSON Lines. Input objects compute and
memoize fields when they are accessed, so constructing a small object does not
first serialize all available commit data. The first implementation will embed
the Rust `jaq` interpreter and guarantee a documented jq-compatible subset
rather than invoking an external program or exposing jj's storage structs.

## Objective

Make jj output straightforward and reliable for programs and agents while
retaining the flexibility which currently makes templates useful.

For example, an agent should be able to request exactly the data it needs:

```console
$ jj log -r 'ancestors(@, 2)' --jq \
    '{change_id, commit_id, description: (.description | split("\n")[0])}'
{"change_id":"...","commit_id":"...","description":"query design"}
{"change_id":"...","commit_id":"...","description":"previous change"}
```

The result has no graph characters, labels, ANSI escapes, or paging. Each line
is one complete JSON value. The user does not have to know how to quote JSON in
a template, remember to append a newline, or pipe a large fixed object through
another process.

## Current state as of jj 0.43.0

Several jj commands accept `-T`/`--template`. The template language is a
statically typed, functional formatting language. It has a rich semantic
`Commit` type, including methods for bookmarks, immutable state, signatures,
and diffs. Template properties are evaluated only when the renderer extracts
them.

The global `json(value)` template function, implemented in [PR
#6777][pr-6777] after the earlier proposal in [issue #5648][issue-5648],
serializes a value. This makes the following possible today:

```console
$ jj log --no-graph -T 'json(self) ++ "\n"'
{"commit_id":"...","parents":["..."],"change_id":"...",...}
```

This is useful, but it has several sharp edges for machine consumers:

- Without `--no-graph`, `jj log` prefixes each value with graph characters.
- `json()` does not add a record separator. Omitting `++ "\n"` concatenates
  adjacent JSON values.
- `json(self)` serializes jj's backend commit representation. It does not expose
  the complete semantic `Commit` template interface, and its compatibility is
  not guaranteed.
- Constructing a JSON object requires hand-written string concatenation and
  escaping. The template language does not currently have general
  heterogeneous object values.
- Every command defines its own top-level template evaluation model. A caller
  must already know whether a template is evaluated once, once per commit, or
  once per diff entry.
- Templates support maps over homogeneous lists, filters, conditionals, and
  string transformations. General heterogeneous object construction,
  dynamic values, grouping, and jq-style stream transformations still require
  another tool and an intermediate schema.

This proposal keeps templates focused on human-readable, labeled output.
Machine-readable output has different defaults and needs a versioned data
contract, explicit framing, and data-language operations.

## Prior work

### jj proposals

[Issue #5662][issue-5662] proposes fixed `--json` output for all commands.
[Issue #3814][issue-3814] discusses composable commands and notes lazy
evaluation as a possible benefit of keeping composition inside a DSL. A
[recent comment][issue-3814-json-comment] also describes the current
`json(self)` framing and semantic-field gaps. [Issue #3219][issue-3219]
proposes a broader structured API and raises an important compatibility
concern: serializing internal structures turns implementation details into a
public API.

[Issue #3262][issue-3262] tracks the design of templates. Its discussion, the
open map-type [issue #7697][issue-7697], and the open object-literal proposals
in [PR #6869][pr-6869] and [PR #8507][pr-8507] show that JSON construction is
useful, but also that heterogeneous objects change the type and evaluation
model of the template language. Merged [PR #8895][pr-8895] made homogeneous
list maps and conditional results serializable, which is useful without
settling the heterogeneous-object design.

Open [PR #6838][pr-6838] proposes an `ndjson()` template alias. Its discussion
surfaces an unresolved question about whether serialization or top-level
iteration should own record separators. This RFC chooses top-level iteration.

[Issue #8738][issue-8738] and [issue #8407][issue-8407] discuss whether a
template receives individual items or a collection. A recent implementation
attempt in [PR #9369][pr-9369] was closed without merging. This design makes
both modes explicit: filters receive one item at a time by default, while
`--slurp` supplies one collection.

The proposals surveyed above do not embed jq or jaq.

### jq and other CLIs

[jq][jq-manual] combines field selection, filtering, object construction, and
stream transformation in a compact language with well-understood semantics.
The jq command applies a filter independently to each input JSON value and may
emit zero, one, or many output values.

The [GitHub CLI][gh-formatting] exposes a related interface: commands select
structured fields with `--json` and optionally transform the result with
`--jq`. That two-stage design is useful prior art, but jj can avoid eagerly
building the intermediate JSON value because its semantic objects are local to
the same process.

### Rust implementations

[jaq][jaq] is an embeddable Rust implementation of jq. It aims for jq
compatibility in most cases and lets hosts implement its value traits for
arbitrary value types. A host value supplies field indexing and iteration, so
jj can expose a commit without first converting the whole commit to
`serde_json::Value`. Evaluating a compiled filter returns a stream of results,
which also fits the streaming behavior of `jj log`.

jaq is not exactly jq. Some filters and command-line features are unsupported,
and its engine does not defend against CPU, memory, or stack exhaustion. This
design therefore defines a jj compatibility contract instead of promising that
every program accepted by a particular jq binary is accepted by jj.

Other implementations are less suitable. [xq][xq] describes itself as under
development and recommends jaq. [jq-rs][jq-rs] wraps C libjq 1.6 and primarily
accepts and returns strings. It offers exact compatibility with that older
libjq version, but does not naturally support a custom lazy jj value and adds a
C build or system-library dependency.

## Goals and non-goals

### Goals

- Produce unambiguous, versioned, machine-readable output.
- Let users construct JSON objects and arrays with familiar jq syntax.
- Evaluate only the semantic fields a filter needs, where jq semantics permit.
- Stream selected commits and filter results without collecting the entire log.
- Preserve revsets and filesets as jj's optimized selection languages.
- Make output order, record framing, failures, and partial output explicit.
- Share semantic field implementations with templates where practical.
- Support enough jq to run common projections, filters, and reductions without
  an external jq executable.

### Non-goals

- Replace templates for human-readable output.
- Make jq a repository mutation language.
- Expose arbitrary Rust or backend storage structures.
- Guarantee all jq command-line options, modules, I/O, or implementation
  details.
- Make every jq operation lazy. Sorting, grouping, random access, and array
  construction inherently retain values.
- Replace revsets with jq. A jq `select()` is a projection-time filter, not a
  substitute for index-backed revision selection.
- Define a stable network or editor RPC protocol. That is the broader scope of
  [issue #3219][issue-3219].
- Accept untrusted filters as safely sandboxed programs.

## Overview

The initial command-line interface is:

```txt
jj log [LOG OPTIONS] --jq <FILTER> [--query-version v1] [--slurp]
```

`jj log` performs its normal repository loading, working-copy snapshot,
revset/fileset evaluation, ordering, and limiting. It then wraps each selected
commit in a `jj.commit/v1` query value and evaluates the compiled filter. The
query version selects both the jq language contract and the input schema; v1
does not have independently moving language and data versions.

Without `--slurp`, evaluation follows jq's ordinary multi-input behavior: the
filter runs once for each selected commit. A filter invocation may emit zero,
one, or many values. Each emitted value is encoded as compact JSON followed by
one line feed.

With `--slurp`, jj first collects the selected commit handles into an array and
runs the filter once. The commit fields inside the array remain lazy, but the
array of handles is not lazy. This is the same important tradeoff as jq's
slurp mode: random access and reordering require retaining the input set.

The filter is compiled before any result is written. The query engine has an
owned, read-only view of the repository and a registry of semantic fields. A
core field is calculated on first access. Namespaced semantic filters cache
either their value or error for that input object.

## Detailed design

### Command-line interface

`--jq <FILTER>` selects structured query output. There is no initial short
option. It conflicts with `--template`, `--patch`, diff-format flags, and
`--count`, all of which select or append another output type.

`--no-graph` is accepted but redundant. Query mode always omits the graph. It
also does not request a pager and never applies color or formatter labels to
standard output, even if `--color=always` is set. Diagnostics on standard error
continue to follow normal UI settings.

Revision and path arguments select commits before query execution. `--limit`
limits commits, not emitted results; `--reversed` reverses that limited stream;
and `--ignore-working-copy` retains its global meaning. Thus
`--limit 10 --jq 'select(.conflict)'` does not walk farther to backfill ten
outputs, while a filter yielding twice per commit can emit twenty.

Query mode uses exactly the ordering of `jj log --no-graph` for the same
revset, paths, limit, and reversal options. In particular, it does not use the
graph-only `revsets.log-graph-prioritize` setting. Existing warnings for path
arguments which match no entries are still written to standard error.

`--query-version <VERSION>` is only valid with `--jq`. The initial and default
value is `v1`. The option makes future language or schema revisions opt-in
without changing existing scripts. A query version freezes the advertised
field set, jq baseline, built-in names and arities, and host-value behavior.

Versions are command-scoped bundles. For `jj log`, v1 means the shared query
language v1 manifest plus the `jj.commit/v1` input schema. Another command can
debut with its own v1 schema without changing `jj log`, and `jj log` can later
add v2 without forcing other commands to do so.

`--slurp` is also only valid with `--jq`. It supplies one array of the selected
commit objects rather than invoking the filter once per commit. It may use
memory proportional to the number of selected commits and whatever data the
filter materializes.

The first version accepts the filter only as a command-line string. Query
files, aliases, positional jq arguments, and raw-output flags can be added
separately without changing the data model.

### Input and result cardinality

The compiled filter runs once per selected commit and can yield zero, one, or
many results. jj preserves commit order and per-filter result order; empty
output is successful. For example:

```console
# zero or one object for each commit
$ jj log --jq \
    'jj::bookmarks as $bookmarks |
     select($bookmarks | length > 0) |
     {commit_id, bookmarks: $bookmarks}'

# one object for each changed file in each commit; bind the parent ID first
$ jj log --jq \
    '.commit_id as $commit_id |
     jj::diff_files[] | {commit_id: $commit_id, path, status}'
```

The filter owns value cardinality. Serialization never inserts array brackets
around multiple values; users who need one array can construct one in
`--slurp` mode.

### Output framing

Every yielded value is written as one compact JSON text followed by `\n`. This
is JSON Lines rather than one top-level JSON document. Strings, numbers,
booleans, and null are valid results and remain JSON encoded:

```console
$ jj log -r @ --jq '.description'
"A description with an escaped newline\n"
```

There is no v1 equivalent of jq's `--raw-output`, `--join-output`, pretty
printing, or color output. Those modes make stream framing dependent on value
types and are easy for callers to misinterpret. A caller which wants text can
decode the JSON string.

jj buffers one yielded value before writing it. Conversion can reject a
non-JSON value; buffering prevents a half-written JSON record. A successful
earlier record can still precede a later runtime failure, so callers must check
the process exit status before treating the stream as complete.

Framing belongs to the command, not to the filter or the `json` serializer.
This resolves the ambiguity discussed in [PR #6838][pr-6838]: a reusable value
serializer should not decide how a top-level iteration separates records.

### jq compatibility contract

The implementation embeds pinned versions of `jaq-core` and its standard
library. Query v1 uses [jq 1.8.2][jq-1.8.2] as its reference behavior. jj
exposes a fixed jq allowlist whose names, arities, values, errors, and
cardinality are tested against that jq release, except for the documented
deviations below.

The v1 user syntax is closed. It permits exactly:

- JSON literals, array and object constructors, object shorthand and computed
  keys, parentheses, comments, string interpolation, and the listed `@` format
  tokens;
- identity `.`, recursive descent `..`, field and array indexing, iteration,
  slices, and the optional `?` suffix;
- pipes, commas, calls, simple `as $name` bindings, variable references, and
  unqualified user `def`s with positional filter arguments;
- `if`/`elif`/`else`, `try`/`catch`, `reduce`, and `foreach`; and
- jq's unary, arithmetic, comparison, boolean, alternative, assignment, and
  update operators.

Destructuring bindings, `label`/`break`, module declarations, and every other
unlisted syntax form are rejected, even if the embedded jaq parser accepts
them. The workspace pins `jaq-parse` as well as the evaluator crates. A
post-parse validator checks the user AST before private library definitions are
injected, and negative conformance tests cover every excluded syntax family.
This prevents a dependency upgrade from silently expanding v1.

The jq portion of the v1 built-in manifest contains these filters:

- Values and objects: `empty`, `type`, `values`, `nulls`, `booleans`,
  `numbers`, `strings`, `arrays`, `objects`, `iterables`, `scalars`, `length`,
  `keys`, `keys_unsorted`, `has`, `contains`, `inside`, `indices`, `index`,
  `rindex`, `select`, `map`, `map_values`, `to_entries`, `from_entries`, and
  `with_entries`.
- Sequences: `range`, `first`, `last`, `nth`, `limit`, `skip`, `any`, `all`,
  `add`, `flatten`, `reverse`, `sort`, `sort_by`, `group_by`, `unique`,
  `unique_by`, `min`, `max`, `min_by`, and `max_by`.
- Strings: `startswith`, `endswith`, `ltrimstr`, `rtrimstr`, `split`, `join`,
  `ascii_downcase`, `ascii_upcase`, and `explode`.
- Conversion and formatting: `tostring`, `tonumber`, `tojson`, `fromjson`,
  `@json`, `@base64`, `@uri`, `@csv`, and `@tsv`.
- Recursion: `recurse`, `walk`, `until`, and `while`.

The implementation must check in `cli/src/query/v1-builtins.toml` as the
normative public manifest. It records only every public jq name and supported
arity and every synthetic `jj` export and arity. A separate checked-in loader
table records whether an entry is native or a library definition and the
private definitions on which it depends; that table is implementation metadata
and may change without a query version when observable behavior does not. The
loader follows only this dependency graph and a curated native-function table.
Private helpers are internally renamed or otherwise unavailable to a top-level
query; loading all definitions exported by `jaq-std` would accidentally expose
excluded functions. Conformance tests must verify both the public manifest
entries and that excluded names fail to compile. A debug command should print
the effective public manifest and dependency versions for bug reports.

The important exclusions are:

| Area | v1 behavior |
| --- | --- |
| `input`, `inputs`, and `--null-input` | Not supported in v1 |
| User modules, `import`, `include`, and search paths | Not supported in v1; the host-provided `jj` namespace is not importable |
| `env`, `$ENV`, filesystem, network, or process access | Not supported |
| `debug`, `stderr`, and other side-channel output | Not supported in v1 |
| `halt` and `halt_error` | Not supported; cannot terminate the host process |
| `now` and other nondeterministic filters | Not supported in v1 |
| Non-finite numbers | Named constructors such as `nan` and `infinite` are unavailable; a literal, conversion, parser, or arithmetic operation which would produce a non-finite number raises a catchable runtime error instead of following jq's normalization behavior |
| Regular-expression filters | Not supported in v1; jaq and jq use different regex dialects |
| `implode` and byte-oriented string construction | Not supported in v1; every query string must remain valid Unicode |
| jq CLI `--arg*`, `--rawfile`, and `--slurpfile` options | Not supported in v1 |
| jq streaming-parser mode | Not applicable to semantic jj inputs |
| Unlisted jq or jaq syntax and filters | Rejected by the v1 AST and built-in allowlists |

This is a jq-compatible subset, not a promise of complete jq compatibility.
The reference is the checked-in public manifest and, for its jq entries,
conformance tests against jq 1.8.2. Synthetic `jj` entries instead use schema
and behavior fixtures. The contract is not whatever happens to be accepted by
a newer jaq dependency. Adding or removing a field or public entry, changing an
arity, or changing observable host-value behavior requires query v2. Bug fixes
which make behavior conform to the stated jq baseline do not.

jj-specific filters use the `jj::` namespace so that they cannot silently
collide with future jq functions. jaq parses this spelling as a module-qualified
call, so jj installs a synthetic module named `jj` during compilation. The
module is always in scope, has no source or filesystem search path, cannot be
imported or shadowed, and exports only the namespaced entries in the v1
manifest. A user definition in the reserved `jj` namespace is a compile error.

### Host values and jq values

Values constructed by a filter are ordinary JSON-like jq values. Input commit
objects are typed host values which present the bounded object interface in the
next section.

Most object operations behave as expected. `.commit_id` indexes a host object,
`keys` lists its advertised fields, and `{id: .commit_id}` constructs an
ordinary object. A missing key returns `null`, so standard jq fallbacks work:

```console
$ jj log -r @ --jq '.future_field // "not available"'
"not available"
```

`has("future_field")` returns false. jj may later add an opt-in strict-field
lint for catching misspellings, but static field access will not change jq's
missing-key semantics in v1.

The advertised fields are deliberately limited to values which are bounded and
infallible after the `Commit` has been loaded. Under standard jq object
operations, a host commit is observationally equivalent to the ordinary JSON
object produced by materializing those fields:

- direct field indexing computes only that field;
- `keys`, `keys_unsorted`, `has`, and `length` use the static field registry;
- `.[]`, `to_entries`, recursive descent, string interpolation,
  `tostring`, `tojson`, comparison, sorting, object addition, and serialization
  materialize the complete bounded object;
- the first assignment, update, or deletion through a host object materializes
  it as an ordinary object and then follows jq's normal missing-key,
  zero-result, and multi-result update behavior.

Because complete materialization cannot invoke fallible repository operations,
jaq's infallible display, equality, and ordering traits can implement structural
jq semantics. Expensive or fallible repository computations are exposed as
explicit `jj::` filters which either return ordinary JSON values or a jq error.

Assignment never changes the repository. An expression such as
`.description = "replacement"` produces an ordinary detached object. The query
environment holds only a read-only repository. That detached object no longer
has commit identity, so applying a `jj::` filter to it is a type error. A query
which needs both forms can bind the original host value before updating it.
Binding, cloning, slurping, reordering, or storing a host value as an element of
an ordinary array or object preserves its identity. Extracting a core field
produces that ordinary field value. Updating or deleting through the host,
using it as an object-addition operand, or a `tojson | fromjson` round trip
materializes a detached ordinary object and loses identity.

### Commit schema v1

The input object has `.schema == "jj.commit/v1"`. It exposes semantic fields,
not the fields produced by Rust `Serialize` derives. Full IDs are used because
shortest IDs depend on the surrounding disambiguation context.

| Field | JSON type | Meaning and cost |
| --- | --- | --- |
| `schema` | string | Always `jj.commit/v1` |
| `commit_id` | string | Full hexadecimal commit ID; cheap |
| `change_id` | string | Full user-facing reverse-hex change ID (`z` through `k` digits); cheap |
| `parent_ids` | array of strings | Direct parent IDs; cheap |
| `description` | string | Stored description verbatim, including any trailing newline |
| `trailers` | array of trailer objects | Parsed description trailers |
| `author` | identity object | Author name, email, and timestamp |
| `committer` | identity object | Committer name, email, and timestamp |
| `conflict` | boolean | Whether the commit tree contains conflicts |
| `root` | boolean | Whether this is the repository root commit |

The object intentionally uses `parent_ids`, not `parents`. If `parents` held
commit objects, serializing `.` could recursively walk all history. Traversing
parent commit objects can be designed later as an explicit operation.

Identity objects contain `name`, `email`, and `timestamp`. The timestamp is an
object with integer `millis_since_epoch` and `utc_offset_minutes` fields. This
preserves every backend timestamp without the fallible conversion to a
calendar date. Trailer objects contain `key` and `value` strings in description
order. The word "identity" distinguishes these values from cryptographic commit
signatures.

The v1 field set is closed. Adding an enumerable field changes `keys`, `.[]`,
comparison, serialization of `.`, execution cost, and possible failures, so it
requires query v2 even if a JSON consumer might otherwise ignore the field.
The table order above is also the v1 host-object insertion order, observable
through `keys_unsorted`, `.[]`, `to_entries`, and serialization. `keys` retains
jq's lexicographic ordering.

This ordering rule applies to every schema-supplied object: tables and inline
field lists in this document declare insertion order. All documented keys are
present, using null where specified, unless the whole object is null. The
`jj log` v1 contract freezes core and namespaced-filter result shapes,
nullability, enum strings, object-key order, and documented array membership
and order. Changing any of them requires `jj log` query v2. `parent_ids` retains
the commit's stored parent order; trailers retain description order.

### jj semantic filters

Repository-derived data is exposed by explicit namespaced filters instead of
enumerable fields. This keeps `.` bounded while retaining demand-driven
evaluation. Every filter below expects a `jj.commit/v1` input and returns an
ordinary JSON value or a jq error:

| Filter | Result | Work performed |
| --- | --- | --- |
| `jj::mine` | boolean | Compare author email with configured user |
| `jj::working_copies` | array | Inspect the repository view |
| `jj::current_working_copy` | boolean | Inspect the current workspace |
| `jj::bookmarks` | reference array | Build or consult the bookmark index |
| `jj::tags` | reference array | Build or consult the tag index |
| `jj::divergent` | boolean | Resolve the commit's change ID |
| `jj::hidden` | boolean | Consult commit visibility |
| `jj::change_offset` | integer or null | Resolve visible change versions |
| `jj::immutable` | boolean | Evaluate the configured immutable policy |
| `jj::empty` | boolean | Compare the commit and parent trees |
| `jj::signature_present` | boolean | Inspect signature presence only |
| `jj::verify_signature` | object or null | Run backend signature verification |
| `jj::diff_files` | array | Walk structural tree changes |
| `jj::diff_stats` | object | Read changed contents and calculate line diffstats |

Namespaced filters participate in normal jq control flow. An expensive filter
in an unselected branch is not called:

```jq
if jj::signature_present then jj::verify_signature else null end
```

Results are memoized per commit and filter. Calling `jj::immutable` twice
evaluates the policy once. Filters which share an underlying index or
signature-verification result also share that work.

`jj::mine` uses exact string equality between the commit author email and the
configured user email. `jj::change_offset` is the non-negative [change
offset](../glossary.md#change-offset): zero identifies the most recent visible
commit for that change ID and larger integers identify successively older
versions. It is null when the input commit cannot be assigned an offset in the
visible change.

`jj::working_copies` contains every workspace whose working-copy commit is the
input commit, sorted lexicographically by workspace name. Bookmark and tag
arrays contain every local or remote reference whose normal, removed, or added
target terms include the input commit, rather than the display-oriented
de-duplication used by some templates. They are sorted lexicographically by
`(name, remote)`, with a null local remote before named remotes. Removed and
added target ID arrays are sorted lexicographically by full commit ID. Here and
below, lexicographic string ordering means Unicode scalar-value order.

A workspace object has this shape:

```json
{"name":"default","current":true}
```

`current` is true exactly when `name` is the workspace running the command.

A reference object has these fields:

| Field | JSON type | Meaning |
| --- | --- | --- |
| `name` | string | Local bookmark or tag name |
| `remote` | string or null | Remote name, or null for a local reference |
| `present` | boolean | Whether the reference has a target |
| `conflict` | boolean | Whether the reference target is conflicted |
| `normal_target_id` | string or null | Non-conflicted target, if present |
| `removed_target_ids` | array of strings | Removed conflict terms |
| `added_target_ids` | array of strings | Added conflict terms |
| `tracked` | boolean | Whether a remote reference is tracked locally |
| `tracking_present` | boolean | Whether its tracking local reference is present |
| `synced` | boolean | Whether local and tracked targets are synchronized |

`present` is false only for a resolved absent target; `conflict` means the
target is unresolved. A resolved normal target has its ID in
`normal_target_id` and empty removed and added arrays. A resolved absent target
has a null normal ID and empty arrays. An unresolved target has a null normal ID
and exposes its simplified, non-null negative and positive merge terms in the
removed and added arrays respectively.

For a local reference, `tracked` and `tracking_present` are false, while
`synced` is true if its target equals every tracked remote target, including
vacuously when there are none. For a remote reference, `tracked` reflects its
tracking state, `tracking_present` additionally requires the local target to be
present, and `synced` requires tracking plus exact target equality. Equality
includes conflicted and absent target structure. Thus an untracked remote has
all three tracking booleans false.

Fields which do not apply have neutral values (`false` or `null`) rather than
changing type. Ahead and behind counts are deliberately not part of this
object: returning an ordinary object would calculate both graph walks even when
a query selects only the reference name. A future explicit filter can expose
counts and their exact-or-estimated contract.

`jj::verify_signature` returns null for an unsigned commit. For a signed commit
it returns:

| Field | JSON type | Meaning |
| --- | --- | --- |
| `status` | string | `good`, `bad`, `unknown`, or `invalid` |
| `key` | string or null | Backend key identifier, if available |
| `display` | string or null | Backend-specific signer display text, if available |

An invalid-format result has status `invalid` and null `key` and `display`.
Other backend failures are jq errors rather than verification objects.

Callers which only need presence use `jj::signature_present`, which does not
invoke GPG, `ssh-keygen`, or other verification work.

`jj::diff_files` describes structural changes from the merged parents to this
commit. It honors path arguments passed to `jj log` and does not load file
contents. Each result contains `path`, `status`, `source`, and `target`.
`status` is `modified`, `added`, or `removed`; v1 deliberately does not run
content-based copy or rename detection. Each non-null side contains `path`,
`file_type`, `executable`, `conflict`, and `conflict_side_count`. A side is null
when absent. For a non-null side, file type is `file`, `symlink`, `tree`,
`git-submodule`, or `conflict`; `executable` is a boolean only for a resolved
`file` and is null for every other type. `conflict` is true exactly when the
simplified entry merge is unresolved, in which case `file_type` is `conflict`.
`conflict_side_count` is the number of positive terms after simplifying that
merge, and is one for a resolved entry. The top-level `path` is the target path
when present and otherwise the source path. Results are sorted by target path,
then source path, then status, using lexicographic repository-path order and
sorting an absent path before a present path.

`jj::diff_stats` calculates line-oriented diffstats from the merged parents to
this commit. It honors path arguments passed to `jj log` and reads the
materialized contents of changed files only when the filter is evaluated. The
result is an object with `files`, `total_added`, and `total_removed`, in that
order. `files` is ordered by repository path and contains objects with these
fields:

| Field | JSON type | Meaning |
| --- | --- | --- |
| `path` | string | Target path, or source path for a removed file |
| `status` | string | `modified`, `added`, or `removed` |
| `lines_added` | integer or null | Added line count, or null for binary content |
| `lines_removed` | integer or null | Removed line count, or null for binary content |
| `bytes_delta` | integer | Target byte size minus source byte size |

`total_added` and `total_removed` sum only non-null per-file line counts, so
binary files do not contribute to either total. As with `jj::diff_files`, v1
does not run content-based copy or rename detection. The per-file object-key
order is `path`, `status`, `lines_added`, `lines_removed`, `bytes_delta`.

### Evaluation and materialization

There are three distinct kinds of laziness:

1. The revset produces commit IDs as a stream in normal mode.
2. The filter produces zero or more results as a stream.
3. A direct field or namespaced semantic filter runs only when jq evaluates
   that expression.

The third property is the important difference from serializing a commit to
JSON before querying it. For this query:

```jq
{change_id, author: .author.email}
```

jj loads only the change ID and author identity metadata. It does not enumerate
bookmarks, resolve immutability, compare trees, verify cryptographic
signatures, or calculate a tree diff.

Each host object owns a shared per-commit cache for direct fields and for the
value or error of each invoked `jj::` filter. Referring to `jj::empty` twice
compares the trees once; `jj::signature_present` and `jj::verify_signature`
share loaded signature metadata. Clones of a host value share this cache. In
normal mode, jj releases it after all results for that input commit have been
written. In `--slurp` mode, retained host values retain their caches.

Laziness does not change jq semantics. These operations can force work:

- Serializing `.` enumerates the bounded core commit fields, but never invokes
  a `jj::` filter.
- `.[]`, `to_entries`, recursive descent, and object update may visit every
  core field of a host object.
- Calling `jj::diff_files`, `jj::diff_stats`, or `jj::verify_signature`
  explicitly performs all work documented for that filter.
- Array construction, `--slurp`, `sort`, `group_by`, `unique`, and random
  indexing retain some or all input values.
- `keys`, `keys_unsorted`, `has`, and `length` can use the field registry
  without computing field values.
- Boolean short-circuiting, `if`, `select`, `//`, and `try` evaluate only the
  branches required by jq semantics.

Objects and arrays constructed by the jq program are ordinary jaq values. The
RFC does not require arbitrary constructed collections to remain lazy.

### Repository consistency

All host objects for one invocation share the `ReadonlyRepo` produced during
command initialization. A field cannot observe a later operation or a
different working-copy snapshot merely because it was evaluated later.

The working copy is snapshotted according to existing command rules before log
selection. Query mode is an output mode and introduces no extra mutation. With
`--ignore-working-copy`, it observes the same repository state as an ordinary
`jj log --ignore-working-copy`.

The environment also owns repository-level indexes reusable across commits and
backend caches whose size is independently bounded. It must not retain every
per-commit query result. Laziness determines when a cache is populated, not
which repository state it represents.

### Errors and exit status

Parsing and compilation finish before the first commit is evaluated. A syntax
error reports a source span on standard error, exits nonzero, and writes no
standard output.

Missing object keys follow jq and return null. Core commit fields are
infallible after the commit is loaded, while a failure in a namespaced filter
is a runtime query error. A pure jq runtime diagnostic identifies the query and
error. An error raised while a `jj::` filter operates on a host commit also
identifies that input commit ID and includes the underlying jj error. A pure jq
error, or a `jj::` type error on a detached value, need not have an associated
commit. jaq does not currently attach a source span to every runtime error, so
v1 promises an exact span only for parse and compile errors.

Catchable errors are observable query values. Pure jq operations produce the
same caught value as jq 1.8.2. A jj-defined error produces this ordinary object,
with keys in the shown order:

```json
{"schema":"jj.query-error/v1","kind":"host","filter":"jj::empty","commit_id":"..."}
```

`kind` is `host` for an underlying repository, configuration, or backend
failure, `type` when a `jj::` filter receives a non-host value, and `non-finite`
for the documented numeric deviation. A `host` object has string `filter` and
`commit_id` values; a `type` object has a string `filter` and null `commit_id`;
a `non-finite` object has null for both. All four keys are always present. The
caught object deliberately omits unstable human-readable source text. If it is
not caught, the standard-error diagnostic includes the underlying jj error
chain. `try`, `catch`, and `?` handle these errors normally.

An unhandled runtime error stops evaluation and exits nonzero. Each earlier
line remains valid JSON, but the stream is incomplete. The current template
behavior of rendering an inline `<Error: ...>` string is not used for
structured output.

A write failure caused by a closed pipe stops evaluation without materializing
additional fields and follows jj's normal broken-pipe exit behavior.
Revset-stream or selected-commit loading failures likewise remain command
errors outside the filter and may occur after earlier records were written.
Cancellation, interruption, those command errors, broken pipes, and internal
invariant failures are host termination conditions, not catchable query errors.

### Resource behavior

Normal per-commit projections keep memory bounded by one input, the query
state, shared repository caches, and one buffered output value. A single result
can still be arbitrarily large.

jaq does not provide a security boundary against resource exhaustion. Filters
can recurse, construct large values, or perform expensive work across many
commits. v1 treats the filter as trusted local input, like a revset or template.
It must stop promptly on a closed output pipe, but it does not claim hard CPU
or memory quotas.

The public mode must still be interruptible. A phase-zero prototype must prove
that jj's interruption check runs during a long pure filter, not only between
yielded results. If stock jaq has no suitable hook, jj must contribute one
upstream or carry a narrow integration patch before enabling `--jq`.

Commands which accept filters from a remote or untrusted source must add their
own policy and resource limits. A later design may add configurable execution,
recursion, materialization, or output budgets; those limits should not silently
truncate valid JSON Lines.

### Encoding

JSON strings are Unicode. The v1 schema only exposes values which jj can
represent as Unicode strings. Repository paths use slash-separated repository
syntax, not platform display syntax. Full file contents and arbitrary backend
byte strings are not part of v1.

Every emitted value must be representable by JSON. Object keys must be strings;
numbers must be finite; and an unprojected commit host value serializes as its
bounded core object. A non-representable value is a runtime error discovered
while buffering that record, so no partial line is written. The v1 manifest
excludes named non-finite constructors, and `QueryValue` rejects non-finite
results from every other numeric path. It also excludes filters which decode
arbitrary bytes.

If byte-valued fields are added later, invalid UTF-8 must not be lossily
replaced. Such a field should be an explicit base64 string or a new query
version with documented byte-string conversion.

## Implementation plan

### Query engine

Add pinned `jaq-core` and `jaq-std` workspace dependencies. `jaq-json` can be a
reference implementation, but its native functions are constrained to
`jaq_json::Val` and cannot simply be loaded for a custom value. Compile filters
with the checked-in v1 manifest and inject the synthetic `jj` module without
enabling jaq's user module loader. If jaq cannot currently do that through a
public API, jj must upstream or carry a narrow compiler extension. Do not invoke
`jq` or `jaq` as a subprocess.

The CLI query module should contain three layers:

1. Parsing, compilation, compatibility filtering, and diagnostics.
2. A `QueryValue` implementation of both [`jaq_core::ValT`][jaq-core-valt]
   and [`jaq_std::ValT`][jaq-std-valt].
3. Command-specific adapters such as `CommitQueryObject`.

`QueryValue` contains ordinary scalar, array, and object values plus bounded
commit host objects. `ValT::index()` resolves only the requested key.
`values()` and `key_values()` materialize each yielded core field to preserve
ordinary jq object semantics. Those iterator APIs can propagate `ValR` errors,
but v1 core getters are infallible; that stronger invariant is needed for the
separate infallible display, equality, and ordering operations. jj-native
implementations of `keys`, `keys_unsorted`, `has`, and `length` consult the
static registry without forcing field values.

`jaq_std::ValT` additionally requires ordering, numeric conversion, sequence,
and byte/string operations. The [`jaq-json` functions][jaq-json-funs]
implemented for `jaq_json::Val`, including `length`, `has`, `contains`,
`fromjson`, and `tojson`, must be generalized upstream or ported to
`QueryValue`. The checked-in manifest makes that porting work finite and
testable.

The `ValT::values()` and `key_values()` child iterators have `'static` outputs,
so directly borrowing the current `CommitTemplateLanguage<'repo>` is
impractical. This design instead lets the query environment own an
`Arc<ReadonlyRepo>`, cloned commits, path conversion state, field registries,
and shared caches. This also makes the repository-snapshot rule explicit; it is
an integration choice, not a requirement that every jaq value own its data.

jaq requires infallible display, equality, and ordering operations for host
values. Those materialize the bounded, infallible core object and apply
ordinary structural jq behavior. Namespaced filters return their errors
directly from a fallible filter call.

### Sharing semantic fields

The commit template method table already defines most desired getters, and
template properties already have a lazy, fallible `extract()` interface. The
proposed shared registry adds explicit cost metadata. The implementation should
extract a common semantic getter layer rather than independently reimplementing
bookmark, immutability, diff, and signature behavior. Sharing applies only
where the contracts match; for example, the v1 diff filters explicitly honor
command-line paths while today's commit template `diff()` defaults to all
paths.

The existing template objects borrow repository state, while this `QueryValue`
design owns it to satisfy its child-iterator lifetime requirements. Directly
wrapping today's boxed template properties is therefore unlikely to work. A
shared registry can instead describe:

- the versioned field or filter name and result type;
- its cost category and documentation;
- an owned getter accepting a query environment and host value;
- adapters into template methods and query fields or filters where contracts
  are the same.

The query schema must not be implemented by deriving `Serialize` on `Commit`.
That repeats the current coupling to backend storage and eagerly walks the
value.

### Async operations

jaq evaluation is synchronous, while some jj store and tree operations are
async. The first implementation can use the same controlled blocking bridge as
current template field extraction. Getters should retain async boundaries so a
future engine can prefetch or await fields without changing the schema.

### Serialization

Convert or serialize one yielded query value into a temporary byte buffer.
Only after the complete value succeeds should jj write the buffer and `\n` to
standard output. This preserves record validity if conversion discovers a
non-JSON value and bounds the extra buffering to one result.

### Delivery order

1. Build a phase-zero prototype without a public flag. It must demonstrate the
   core and standard `ValT` implementations, the ported v1 built-ins, structural
   host updates and comparison, synthetic-module resolution and shadowing
   rejection for fallible namespaced filters, complete buffered serialization,
   and interruption inside a non-yielding filter.
2. Check in the jq 1.8.2 conformance corpus and exact v1 function manifest.
3. Implement the closed `jj.commit/v1` core object and every v1 namespaced
   filter, still behind an internal experimental command.
4. Add `--slurp`, collection tests, and the user documentation.
5. Expose `jj log --jq` and query version v1 only after all compatibility and
   schema fixtures pass.

An internal prototype may change freely, but it must not advertise itself as
query v1. The public v1 contract consists of this document plus the exact
`cli/src/query/v1-builtins.toml` manifest checked in before the public flag
ships.

### Testing

The conformance suite should run the jq portion of the v1 manifest through jq
1.8.2 and jj's engine and compare values and errors. Synthetic `jj` exports use
repository fixtures instead. Tests should cover construction, cardinality,
updates, optional access, reductions, numeric edges, and Unicode.
Negative cases should cover every excluded syntax family, function, and arity.
Caught-error fixtures should separately freeze jq-compatible values and
`jj.query-error/v1` objects. Counting getters should verify direct-field
laziness and memoization; cached filter values and errors; non-forcing `keys`,
`keys_unsorted`, `has`, and `length`; bounded materialization for comparison,
serialization, and updates; and short-circuiting around expensive filters.

CLI snapshots should cover clean JSON Lines for every value type and
cardinality; selection, flat ordering, limiting, reversal, paths, and slurp;
parse and runtime failures, invalid JSON-domain values, and broken pipes; and
stable full IDs and raw timestamp/offset pairs.

## Alternatives considered

### Fixed JSON and two-stage field selection

A fixed schema, as proposed by [issue #5662][issue-5662], is simpler and can be
consumed by any JSON tool. The GitHub CLI's `--json <fields> --jq <filter>`
variant also makes field costs explicit. Both remain reasonable complements to
this proposal.

On their own, they duplicate field selection or require an external step for
renaming, filtering, flattening, and one-to-many output. A jq filter describes
the demanded core fields and result shape together, while expensive `jj::`
filters remain explicit.

### Extend templates with object and array literals

[PR #6869][pr-6869] and [PR #8507][pr-8507] explore this incremental path. It
avoids a new dependency and preserves static template type checking. Object and
array literals would also improve templates independently of this RFC.

Object construction alone does not settle filtering, reductions, dynamic
values, update semantics, aggregate inputs, error handling, or output framing.
Growing all of those features would create another data language for users and
maintainers to learn. jq already has those semantics and an embeddable Rust
implementation.

### Pipe `json(self)` to external jq

This works today for the backend commit fields if callers suppress the graph
and add separators correctly. It is a useful fallback, but it exposes the wrong
schema, lacks computed semantic fields, adds process and installation
requirements, and must serialize the intermediate value before jq can discard
fields.

### Embed C libjq or define a new language

libjq provides exact behavior for the library version being embedded, but the
available Rust binding targets libjq 1.6 rather than this RFC's jq 1.8.2
baseline. It also uses a string-oriented API, so jj would eagerly serialize
input or need a substantial C integration for host values, in addition to a
native C dependency and system-library or bundled build. jaq's custom value
interface is a better fit for demand-driven semantic filters.

An independent jq-inspired language could integrate tightly with jj's types,
but small differences in null handling, cardinality, iteration, and errors
would make familiar-looking filters surprising. jj would also own a new parser,
evaluator, standard library, and documentation.

### Eager JSON and a general API

Eagerly converting the bounded core object would be inexpensive and would make
interpreter integration simpler. It would lose the typed commit identity which
`jj::bookmarks`, `jj::empty`, and other semantic filters need after jq values
are cloned, slurped, or updated. A hidden side table or token could restore that
identity, but would give projected ordinary objects surprising capabilities.
The custom host value keeps identity explicit while still materializing to a
normal object whenever jq requires it. A stable RPC API would better serve
long-running clients, but adds transport, lifecycle, subscription, and mutation
questions. The curated query schema can later inform such an API.

## Open questions

- Should the public flag be `--jq`, which identifies the language precisely,
  or `--query`, which leaves room for other implementations? This RFC proposes
  `--jq` because it sets the clearest user expectation.
- Should the custom-value functions be generalized in jaq upstream or ported
  into jj? The behavior and conformance requirements are the same either way,
  but upstream generalization reduces long-term maintenance.
- What is the narrowest jaq interruption hook that can check jj's cancellation
  state without imposing significant overhead on every filter step?
- Should later versions support raw string output? If so, the mode must remain
  explicit and must not weaken the default JSON Lines contract.

## Related work

The [jq manual][jq-manual] defines the language model users will bring to this
feature. [jaq's manual][jaq-manual] documents known implementation differences.
jaq's custom-value interface has also been used to expose non-JSON host data,
including an [Amazon Ion integration][ion-jaq], which is close to jj's proposed
semantic values.

The jaq maintainer's [discussion of lazy inputs][jaq-lazy-input] distinguishes
streaming independent inputs, which jaq supports, from a generally lazy
slurped array, which random access and sharing make impractical. That is why
this RFC streams by default and describes `--slurp` as collecting commit
handles while leaving only their core fields demand-driven.

The GitHub CLI's [`--json` and `--jq` interface][gh-formatting] demonstrates the
value of making jq available without requiring an external executable. This
RFC differs mainly in moving field selection into a lazy host object and in
making JSON Lines framing explicit.

## Future possibilities

Once the query engine and schema discipline are established, the same model can
support other item-oriented commands:

- `jj evolog --jq` with a versioned evolution-entry object;
- `jj op log --jq` with an operation object;
- `jj bookmark list --jq` and `jj workspace list --jq`;
- `jj diff --jq` with one top-level diff object instead of per-entry templates;
- content-reading copy detection with separately frozen similarity semantics;
- explicit bookmark tracking-count filters with frozen estimate semantics;
- a fixed `--json` shorthand implemented as a stable identity projection;
- query files, aliases, `--arg` values, and additional namespaced filters in a
  new query version;
- regular-expression filters with a separately frozen dialect and a
  Unicode-safe `implode` implementation;
- streaming reductions over all inputs without constructing a slurped array;
- a schema introspection command for agents and completion systems;
- cost metadata or budgets for expensive fields;
- editor and RPC APIs which reuse the same semantic schema.

These commands should share the query language and framing rules, but each must
define its own stable input schema rather than serialize an internal type.

[gh-formatting]: https://cli.github.com/manual/gh_help_formatting
[ion-jaq]: https://github.com/amazon-ion/ion-cli/pull/193
[issue-3219]: https://github.com/jj-vcs/jj/issues/3219
[issue-3262]: https://github.com/jj-vcs/jj/issues/3262
[issue-3814]: https://github.com/jj-vcs/jj/issues/3814
[issue-3814-json-comment]: https://github.com/jj-vcs/jj/issues/3814#issuecomment-4519489318
[issue-5648]: https://github.com/jj-vcs/jj/issues/5648
[issue-5662]: https://github.com/jj-vcs/jj/issues/5662
[issue-7697]: https://github.com/jj-vcs/jj/issues/7697
[issue-8407]: https://github.com/jj-vcs/jj/issues/8407
[issue-8738]: https://github.com/jj-vcs/jj/issues/8738
[jaq-core-valt]: https://docs.rs/jaq-core/latest/jaq_core/val/trait.ValT.html
[jaq-json-funs]: https://docs.rs/jaq-json/latest/jaq_json/fn.funs.html
[jaq-lazy-input]: https://github.com/01mf02/jaq/issues/276
[jaq-manual]: https://gedenkt.at/jaq/manual/
[jaq-std-valt]: https://docs.rs/jaq-std/latest/jaq_std/trait.ValT.html
[jaq]: https://github.com/01mf02/jaq
[jq-1.8.2]: https://github.com/jqlang/jq/releases/tag/jq-1.8.2
[jq-manual]: https://jqlang.github.io/jq/manual/
[jq-rs]: https://github.com/onelson/jq-rs
[pr-6838]: https://github.com/jj-vcs/jj/pull/6838
[pr-6869]: https://github.com/jj-vcs/jj/pull/6869
[pr-6777]: https://github.com/jj-vcs/jj/pull/6777
[pr-8507]: https://github.com/jj-vcs/jj/pull/8507
[pr-8895]: https://github.com/jj-vcs/jj/pull/8895
[pr-9369]: https://github.com/jj-vcs/jj/pull/9369
[xq]: https://github.com/MiSawa/xq
