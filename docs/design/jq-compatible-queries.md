# jq-compatible structured queries

Author [Adam Hupp](mailto:adamh@openai.com)

With `jj log --jq <FILTER>`, a script can ask for the ID and subject of each
commit, list its changed files, or select only commits with bookmarks. The
filter uses a subset of jq. Each result occupies one JSON line.

jj reads commit fields as the filter needs them. Looking up bookmarks,
verifying a signature, or computing a diff requires an explicit `jj::` filter.

The branch exposes `--jq`, `--query-version v1`, and `--slurp`. This RFC
specifies the v1 contract. Several checks still need work, including source
locations in parse errors and interruption of long calculations.
[Implementation status](#implementation-status)

## What users can ask

### Choose fields and shape the result

A program can request full IDs and the first line of each description.

```console
$ jj log -r 'ancestors(@, 2)' --jq \
    '{change_id, commit_id, description: (.description | split("\n")[0])}'
{"change_id":"...","commit_id":"...","description":"query design"}
{"change_id":"...","commit_id":"...","description":"previous change"}
```

Each line is a complete JSON value. jj adds the record separator and omits graph
characters, labels, color, and paging. The caller does not need to construct
JSON strings in a template or serialize a large object for another process.

### Select results or produce several results per commit

A filter runs once per selected commit by default. It can emit zero, one, or
many values. The first query selects commits with bookmarks. The second emits
one object per changed file.

```console
# zero or one object for each commit
$ jj log --jq \
    'jj::bookmarks as $bookmarks |
     select($bookmarks | length > 0) |
     {commit_id, bookmarks: $bookmarks}'

# one object for each changed file in each commit
# keep the commit ID before iterating over file entries
$ jj log --jq \
    '.commit_id as $commit_id |
     jj::diff_files[] | {commit_id: $commit_id, path, status}'
```

jj keeps commit order and the order of values from each filter invocation.
Empty output is successful. It does not automatically wrap several results in
an array.

Revsets and filesets select commits before the filter runs. They remain the
optimized way to select revisions. A jq `select()` can make further choices
while constructing output.

### Work with a collection

`--slurp` collects the selected commit handles into an array and invokes the
filter once. This permits grouping, sorting, reductions, and a single array
result. Core fields still load only when read, but jj must retain the handles
for random access and reordering. Memory use grows with the selected commits
and the values the filter computes.

### Request expensive information only when needed

Core fields describe the loaded commit. The `jj::` filters provide information
that needs repository queries or can fail. A branch that jq skips does not run
its filters.

```jq
select(jj::signature_present) | jj::verify_signature
```

The presence check does not run GPG or `ssh-keygen`. Verification runs only for
a signed commit. An unsigned commit produces no output. jj caches the result
or error for each commit.

Templates continue to serve labeled output for people. This interface serves
programs that need a stable data contract and transformations. It does not
change the repository, replace revsets, define an RPC protocol, or sandbox
untrusted filters.

## Command and output contract

### Command-line interface

```txt
jj log [LOG OPTIONS] --jq <FILTER> [--query-version v1] [--slurp]
```

`--jq <FILTER>` has no short option in v1. It conflicts with `--template`,
`--patch`, diff-format flags, and `--count`, which select or append other output.
The initial interface accepts a command-line filter string only.

Revision and path arguments select commits under the existing `jj log` rules.
`--limit` limits input commits. `--reversed` reverses that limited stream. For
example, `--limit 10 --jq 'select(.conflict)'` examines at most ten commits.
It does not search until it finds ten matches. A filter that emits two values
per commit can produce twenty results from those ten commits.

Output order matches `jj log --no-graph` with the same revset, paths, limit, and
reversal options. Query mode ignores `revsets.log-graph-prioritize`, which
applies only to graph output. It accepts `--no-graph` and always omits the graph.
Warnings about path arguments that match no entries still go to standard error.

jj does not request a pager. Standard output has no color or formatter labels,
even with `--color=always`. Diagnostics on standard error use normal UI settings.

Both `--query-version` and `--slurp` require `--jq`. The first and default query
version is `v1`. Scripts must opt into a later version. Query files, aliases,
positional jq arguments, and explicit raw-output modes are possible later work.

### JSON Lines

jj writes each result as compact JSON followed by one line feed, `\n`.
Strings, numbers, booleans, and null are valid results.

```console
$ jj log -r @ --jq '.description'
"A description with an escaped newline\n"
```

v1 does not provide jq's `--raw-output`, `--join-output`, pretty printing, or
color output. Callers that need text can decode the JSON string. The command adds the separator after it writes each value.

jj converts one complete result into a temporary byte buffer before writing
it. A conversion failure therefore writes none of that record. A later runtime
error can still follow earlier valid lines. Callers must check the process
exit status before treating the stream as complete.

### Versions and stability

For `jj log`, query v1 combines the shared language v1 manifest with the
`jj.commit/v1` input schema. The language and schema do not have independent
versions within this command. Other commands version their contracts
separately. They can introduce their own v1 schema, or retain v1 when `jj log`
adds v2.

The contract fixes public function names and arities, core fields, host-value
behavior, result shapes, nullability, enum strings, object-key order, and the
specified membership and order of arrays. Adding or removing a field or public
entry requires v2. Changing any other part of this contract also requires v2.
A fix that restores the documented jq behavior does not require a new version.

An additional enumerable field needs v2 even if a consumer could ignore it.
It changes `keys`, `.[]`, comparison, serialization of `.`, and execution cost.

Every schema object follows the insertion order of its table or inline field
list in this document. All documented keys are present, with null where
specified, unless the whole object is null. `keys` keeps jq's lexicographic
order. Lexicographic string order means Unicode scalar-value order.

### Repository state

jj loads the repository, snapshots the working copy under the existing command
rules, and selects commits before evaluating the query. `--ignore-working-copy`
keeps its global meaning. Query output observes the same state as ordinary
`jj log --ignore-working-copy` when that flag is set.

All host values share the `ReadonlyRepo` created during command initialization.
Reading a field later must not observe a later operation or a different
working-copy snapshot. The query adds no repository mutation.

## Commit data and semantic filters

### Host values and jq values

An input commit is a typed host value. The interpreter exposes it as an object
with a fixed set of core fields and retains its commit identity for `jj::`
filters. A filter can also construct ordinary jq scalars, arrays, and objects.

`.commit_id` reads one core field. `{id: .commit_id}` builds an ordinary object.
A missing key returns null, so jq fallbacks work.

```console
$ jj log -r @ --jq '.future_field // "not available"'
"not available"
```

`has("future_field")` is false. A future strict-field lint could catch spelling
mistakes, but v1 preserves jq's missing-key behavior.

Binding, cloning, slurping, and reordering a host value preserve its commit
identity. Putting it inside an ordinary array or object also preserves that
identity. Reading a core field returns an ordinary value.

The first assignment, update, or deletion through a host object converts it to
an ordinary object. The operation then follows jq's missing-key, zero-result,
and multi-result update rules. Object addition and a `tojson | fromjson` round
trip also remove commit identity.

For example, `.description = "replacement"` returns a detached object. It does
not change the repository. A `jj::` filter rejects that object with a type
error. A query that needs the original commit and the changed object can bind
the host value before the update.

### Commit schema v1

The `.schema` field is `"jj.commit/v1"`. Once jj loads a `Commit`, it can
read the core fields without another repository lookup or a fallible operation.

| Field | JSON type | Meaning and cost |
| --- | --- | --- |
| `schema` | string | Always `jj.commit/v1` |
| `commit_id` | string | Full hexadecimal commit ID |
| `change_id` | string | Full change ID in jj’s reverse-hex alphabet (`z` through `k`) |
| `parent_ids` | array of strings | Direct parent IDs in stored order |
| `description` | string | Stored description verbatim, including any trailing newline |
| `trailers` | array of trailer objects | Parsed description trailers |
| `author` | identity object | Author name, email, and timestamp |
| `committer` | identity object | Committer name, email, and timestamp |
| `conflict` | boolean | Whether the commit tree contains conflicts |
| `root` | boolean | Whether this is the repository root commit |

IDs are full-length. A shortest unambiguous ID depends on other repository IDs.
`parent_ids` keeps stored parent order and contains only IDs. Exposing parent
commit objects in `.` could make serialization walk all history. Parent
traversal would need an explicit operation in a later design.

Identity objects contain `name`, `email`, and `timestamp`, in that order. A
timestamp contains integer `millis_since_epoch` and `utc_offset_minutes` fields.
These preserve every backend timestamp without a calendar conversion that could
fail. The signature filter handles cryptographic verification separately. Trailer objects contain `key` and `value` strings in description
order.

Direct indexing computes only the requested field. `keys`, `keys_unsorted`,
`has`, and `length` use the field registry without computing values. Operations that need the whole object compute all core fields. Examples include
`.[]`, `to_entries`, recursive descent, interpolation, `tostring`, `tojson`,
comparison, sorting, addition, updates, and serialization. None invokes a
`jj::` filter.

Infallible core fields let jaq's display, equality, and ordering traits behave
like those of an ordinary jq object. Repository operations that can fail belong
in explicit semantic filters.

### jj semantic filters

Every filter below expects a `jj.commit/v1` host input. It returns an ordinary
JSON value or a jq error. The filters are not enumerable core fields, so
serializing `.` does not request their work.

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

Each host value caches the value or error from each filter. Calling
`jj::immutable` twice evaluates the policy once. Filters share underlying
indexes and signature-verification work where applicable.

`jj::mine` compares the author email with the configured user email by exact
string equality. `jj::change_offset` returns the non-negative
[change offset](../glossary.md#change-offset). Zero is the newest visible commit
for that change ID. Larger values identify successively older versions. The
result is null when the input commit cannot be assigned an offset in the visible
change.

#### Workspaces

`jj::working_copies` includes every workspace whose working-copy commit is the
input commit. It sorts the array by workspace name. Each object has the fields
shown below, in this order.

```json
{"name":"default","current":true}
```

`current` is true exactly when `name` is the workspace running the command.

#### Bookmarks and tags

`jj::bookmarks` and `jj::tags` include every local or remote reference whose
normal, removed, or added target terms include the input commit. They retain
references that some templates remove as display duplicates. Arrays are sorted
by `(name, remote)`, with a local reference's null remote before named remotes.
Removed and added target arrays are sorted by full commit ID.

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

A resolved absent target has `present` false, a null normal ID, and empty
removed and added arrays. A resolved normal target has its ID in
`normal_target_id` and empty arrays. An unresolved target has `conflict` true
and a null normal ID. Its removed array contains the simplified, non-null
negative merge terms. Its added array contains the corresponding positive
terms. `present` is false only for a resolved absent target.

For a local reference, `tracked` and `tracking_present` are false. `synced` is
true when its target equals every tracked remote target, including when there
are no tracked remotes.

For a remote reference, `tracked` reports its tracking state.
`tracking_present` requires tracking and a present local target. `synced`
requires tracking and exact target equality, including the structure of
conflicted or absent targets. All three are false for an untracked remote.
Fields that do not apply keep their declared type and use the specified false
or null value.

The object has no ahead or behind counts. Returning them in an ordinary object
would require both graph walks even when a query selects only a name. A future
explicit filter could provide counts and specify whether they are exact or
estimated.

#### Signatures

`jj::verify_signature` returns null for an unsigned commit. For a signed
commit, it returns this object.

| Field | JSON type | Meaning |
| --- | --- | --- |
| `status` | string | `good`, `bad`, `unknown`, or `invalid` |
| `key` | string or null | Backend key identifier, if available |
| `display` | string or null | Backend-specific signer display text, if available |

An invalid-format result has status `invalid` and null `key` and `display`.
Other backend failures produce jq errors. `jj::signature_present` only checks
presence and does not run verification.

#### Diffs

Both diff filters compare the merged parents with the input commit and honor
path arguments passed to `jj log`. v1 does not detect copies or renames from
file contents.

`jj::diff_files` walks structural changes without loading file contents. Each
result contains `path`, `status`, `source`, and `target`, in that order.
`status` is `modified`, `added`, or `removed`. An absent side is null. A present
side contains `path`, `file_type`, `executable`, `conflict`, and
`conflict_side_count`, in that order.

`file_type` is `file`, `symlink`, `tree`, `git-submodule`, or `conflict`.
`executable` is a boolean only for a resolved `file` and is null otherwise.
`conflict` is true exactly when the simplified entry merge is unresolved. Its
`file_type` is then `conflict`. `conflict_side_count` counts positive terms
after merge simplification and is one for a resolved entry.

The top-level path is the target path when present, or the source path
otherwise. Results sort by target path, then source path, then status. Paths use
lexicographic repository-path order, with absent paths before present paths.

`jj::diff_stats` reads the materialized contents of changed files when called.
Its result contains `files`, `total_added`, and `total_removed`, in that order.
`files` is sorted by repository path and contains these objects.

| Field | JSON type | Meaning |
| --- | --- | --- |
| `path` | string | Target path, or source path for a removed file |
| `status` | string | `modified`, `added`, or `removed` |
| `lines_added` | integer or null | Added line count, or null for binary content |
| `lines_removed` | integer or null | Removed line count, or null for binary content |
| `bytes_delta` | integer | Target byte size minus source byte size |

The totals sum non-null per-file line counts. Binary files do not contribute to
either total. File keys follow the table order.

## jq language v1

### Supported syntax and functions

Query v1 uses [jq 1.8.2][jq-1.8.2] as the reference for its supported subset.
It embeds pinned jaq crates and does not invoke an external jq or jaq program.
The exact public names and arities belong in
[`cli/src/query/v1-builtins.toml`](../../cli/src/query/v1-builtins.toml).
Conformance tests must compare values, errors, and result counts with jq 1.8.2.
The exceptions below are part of the contract.

v1 accepts these syntax families.

- JSON literals, array and object constructors, shorthand and computed keys,
  parentheses, comments, string interpolation, and the listed `@` format tokens.
- Identity `.`, recursive descent `..`, field and array indexing, iteration,
  slices, and the optional `?` suffix.
- Pipes, commas, calls, simple `as $name` bindings, variable references, and
  unqualified user `def`s with positional filter arguments.
- `if`/`elif`/`else`, `try`/`catch`, `reduce`, and `foreach`.
- jq unary, arithmetic, comparison, boolean, alternative, assignment, and update
  operators.

Destructuring bindings, `label`/`break`, module declarations, and every unlisted
syntax family are rejected, even if jaq's parser accepts them. jj must validate
the user's abstract syntax tree before adding private library definitions.
The parser and evaluator versions must both be pinned. Negative tests must
prevent a dependency upgrade from silently expanding v1.

The manifest contains the following jq filters. It specifies each supported
arity as well as its name.

| Group | Filters |
| --- | --- |
| Values and objects | `empty`, `type`, `values`, `nulls`, `booleans`, `numbers`, `strings`, `arrays`, `objects`, `iterables`, `scalars`, `length`, `keys`, `keys_unsorted`, `has`, `contains`, `inside`, `indices`, `index`, `rindex`, `select`, `map`, `map_values`, `to_entries`, `from_entries`, `with_entries` |
| Sequences | `range`, `first`, `last`, `nth`, `limit`, `skip`, `any`, `all`, `add`, `flatten`, `reverse`, `sort`, `sort_by`, `group_by`, `unique`, `unique_by`, `min`, `max`, `min_by`, `max_by` |
| Strings | `startswith`, `endswith`, `ltrimstr`, `rtrimstr`, `split`, `join`, `ascii_downcase`, `ascii_upcase`, `explode` |
| Conversion and formatting | `tostring`, `tonumber`, `tojson`, `fromjson`, `@json`, `@base64`, `@uri`, `@csv`, `@tsv` |
| Recursion | `recurse`, `walk`, `until`, `while` |

### Excluded features

| Area | v1 behavior |
| --- | --- |
| `input`, `inputs`, and `--null-input` | Not supported |
| User modules, `import`, `include`, and search paths | Not supported. The host-provided `jj` namespace is not importable |
| `env`, `$ENV`, filesystem, network, or process access | Not supported |
| `debug`, `stderr`, and other side-channel output | Not supported |
| `halt` and `halt_error` | Not supported. Queries cannot terminate the host process |
| `now` and other nondeterministic filters | Not supported |
| Non-finite numbers | Constructors such as `nan` and `infinite` are unavailable. A literal, conversion, parser, or arithmetic operation that would produce a non-finite number raises a catchable runtime error. This differs from jq's normalization behavior |
| Regular-expression filters | Not supported. jaq and jq use different regex dialects |
| `implode` and byte-oriented string construction | Not supported. Every query string must remain valid Unicode |
| jq CLI `--arg*`, `--rawfile`, and `--slurpfile` | Not supported |
| jq streaming-parser mode | Not applicable to semantic jj inputs |
| Any unlisted jq or jaq syntax or filter | Rejected by the AST or built-in allowlist |

### The `jj` namespace

jaq parses `jj::` as a module-qualified call. jj installs a synthetic module
named `jj` during compilation and keeps it in scope. It has no source or
filesystem search path. Users cannot import or shadow it. A definition in the
reserved `jj` namespace is a compile error. The module exports only the public
names and arities in the v1 manifest.

The public manifest must contain no implementation details. A separate loader
table records native functions, library definitions, and private dependencies.
It may change without a new query version when observable behavior stays the
same. The loader must follow only those dependencies and a fixed native-function
table. Private helpers must be renamed internally or kept unavailable by another
method. Loading all of `jaq-std` would expose excluded functions.

Tests must exercise every public manifest entry and reject excluded names and
arities. A debug command should print the effective public manifest and
dependency versions for bug reports.

## Evaluation, errors, and limits

### Evaluation and materialization

Normal mode streams revset commit IDs and the results of each filter. A core
field or `jj::` filter runs when jq evaluates its expression.

```jq
{change_id, author: .author.email}
```

This query loads the change ID and author metadata. It does not enumerate
bookmarks, evaluate immutability, compare trees, verify signatures, or calculate
a diff.

Clones of a host value share its cache of core fields and semantic-filter
values or errors. Reading `jj::empty` twice compares the trees once.
`jj::signature_present` and `jj::verify_signature` share signature metadata.
Normal mode releases the per-commit cache after writing that input's results.
Slurped host values retain their caches.

The environment may reuse repository indexes and backend caches across commits.
Backend caches keep their own size limits. The environment must not retain
every per-commit query result. All cached values refer to the same repository
state.

Lazy evaluation must preserve jq behavior. Boolean short-circuiting, `if`,
`select`, `//`, and `try` evaluate only the branches jq requires. Calling a
semantic filter explicitly performs all its documented work. Array construction,
`--slurp`, `sort`, `group_by`, `unique`, and random indexing retain some or all
input values. Objects and arrays constructed by a query have ordinary jaq
behavior and have no general lazy-evaluation guarantee.

### Errors and exit status

jj must parse and compile the filter before evaluating the first commit. A
parse or compile error must identify the exact source span on standard error
and exit nonzero without writing standard output. Runtime errors need not have
exact spans because jaq does not attach one to every runtime error.

A pure jq runtime diagnostic identifies the query and error. A failing `jj::`
filter on a host commit also identifies the input commit ID and underlying jj
error. A pure jq error or a `jj::` type error on a detached value need not have
an associated commit.

Caught errors from pure jq operations must have the same values as jq 1.8.2.
A jj-defined error is an ordinary object with this key order.

```json
{"schema":"jj.query-error/v1","kind":"host","filter":"jj::empty","commit_id":"..."}
```

All four keys are present. Their values depend on the failure.

| `kind` | Cause | `filter` | `commit_id` |
| --- | --- | --- | --- |
| `host` | Repository, configuration, or backend failure | Filter name string | Full commit ID string |
| `type` | A `jj::` filter received a non-host value | Filter name string | null |
| `non-finite` | An operation would produce a non-finite number | null | null |

The caught object omits human-readable source text, which can change. An
uncaught error includes the underlying jj error chain in the standard-error
diagnostic. `try`, `catch`, and `?` handle query errors normally.

An unhandled runtime error stops evaluation and exits nonzero. Earlier records
remain valid JSON, but the stream is incomplete. Query output does not insert
the `<Error: ...>` strings that templates can render.

Revset-stream failures and failures to load a selected commit are command errors
outside the filter. They can occur after earlier records. A closed pipe stops
evaluation without computing more fields and follows normal jj broken-pipe exit
behavior. Queries cannot catch these command errors, broken pipes, cancellation,
interruption, or internal invariant failures. They terminate the command.

### Resource behavior

A normal per-commit projection retains one input, query state, shared repository
caches, and one buffered result. A single result can be arbitrarily large.
Slurping and collection operations can retain much more data.

jaq does not prevent CPU, memory, or stack exhaustion. Filters can recurse or
construct large values. v1 treats filters as trusted local input, like revsets
and templates. It provides no hard CPU or memory quota. Commands that accept
remote or untrusted filters must provide their own policy and resource limits.
Later execution, recursion, materialization, or output limits must not silently
truncate valid JSON Lines.

Public query mode must support interruption during a long pure filter that does
not yield results. Checking only between results is insufficient. The
phase-zero prototype must demonstrate this before the public flag is enabled.
If jaq has no suitable hook, jj must add one upstream or carry a small
integration patch. A closed output pipe must also stop evaluation promptly.

### Encoding

Strings must remain valid Unicode. Repository paths use `/` on every platform. v1 excludes full file contents
and arbitrary backend bytes. Object keys must be strings and numbers must be
finite. A host commit serializes as its core object.

`QueryValue` must reject non-finite results from all numeric operations, including
those outside the named constructors excluded by the manifest. Filters that
decode arbitrary bytes are also excluded. The per-record buffer prevents an
unrepresentable value from producing a partial line.

A later byte-valued field must preserve invalid UTF-8 bytes. It should use an
explicit base64 string, or a new query version must define byte-string
conversion.

## Implementation

### Implementation status

The local `dev/adamh/jq-query` implementation has the command-line flags, typed
commit values, cached core fields, the fourteen semantic filters, a synthetic
`jj` module, AST checks, and per-result buffering. The command uses one
`Arc<ReadonlyRepo>`, streams commits in normal mode, and collects commit handles
for slurp.

The workspace pins `jaq-core` 3.1.0, `jaq-json` 2.0.1, and `jaq-std` 3.0.1.
`jaq-core` also supplies the parser.

The branch still needs the following work to meet the v1 contract.

- `QueryProgram::compile()` and source validation use function lists in Rust.
  The checked-in `v1-builtins.toml` and `v1-loader.toml` do not drive compilation.
  They need a checked connection to the effective public function set.
- v1 accepts JSON literals. The current compiler rejects the filter `null`,
  including inside `[null]` and `{value: null}`. The definitions loaded by jj
  omit jaq's definition for `null`. Fields can still return a JSON null.
- The initial parse failure in `validate_source()` returns a generic syntax
  error and discards the source span. It does not meet the exact-span promise.
- The query evaluator and result loop have no explicit interruption check.
  Interruption during a pure filter that does not yield remains unproved.
- The checked-in tests cover a basic query, some rejected syntax, manifest
  parsing, JSON Lines, slurp, semantic-filter smoke cases, diffstats, and option
  conflicts. They do not establish the full jq 1.8.2 differential conformance,
  getter-call counts, or all schema and failure fixtures required below.
- The proposed common getter registry with templates is not implemented.
  Query-specific functions currently provide semantic-filter behavior.

### Query engine

The design has three layers.

1. Parse and compile filters, restrict compatibility, and report diagnostics.
2. Implement `QueryValue` through [`jaq_core::ValT`][jaq-core-valt] and
   [`jaq_std::ValT`][jaq-std-valt].
3. Adapt command inputs through types such as `CommitQueryObject`.

`QueryValue` holds ordinary scalars, arrays, and objects, plus commit host
objects. `ValT::index()` reads one core field. `values()` and `key_values()`
compute the fields they yield. Those iterator APIs can return `ValR` errors,
but v1 core getters must remain infallible for display, equality, and ordering.
`keys`, `keys_unsorted`, `has`, and `length` use field names without computing
values.

`jaq_std::ValT` also requires ordering, numeric conversion, sequence operations,
and byte and string operations. Native [`jaq-json` functions][jaq-json-funs]
such as `length`, `has`, `contains`, `fromjson`, and `tojson` require
`jaq_json::Val`. They must be generalized upstream or ported for `QueryValue`.
The manifest defines the exact functions that need implementation and tests.

The child iterators from `values()` and `key_values()` return `'static` values.
They cannot directly borrow the current `CommitTemplateLanguage<'repo>`.
The query environment therefore owns an `Arc<ReadonlyRepo>`, cloned commits,
path conversion state, field registries, and shared caches. This also keeps all
inputs on one repository snapshot.

Compile with the v1 manifest and install the synthetic `jj` module without
user-module loading. If jaq's public API cannot do this, contribute a small
compiler extension or maintain one in jj. The engine must not run a subprocess.

### Sharing semantic fields

The template method table already provides most needed getters. Template
properties have a lazy, fallible `extract()` interface. A common layer could
share bookmark, immutability, diff, and signature behavior and add cost metadata.
Getters should be shared only when their contracts match. Query diff filters
honor command-line paths, while the current commit template `diff()` defaults
to all paths.

Template objects borrow repository state. Query values own it for their child
iterators. Directly wrapping boxed template properties is therefore unlikely to
work. The proposed registry describes the versioned name, result type, cost
category, documentation, and an owned getter that accepts the environment and
host value. Adapters expose that getter to templates and queries where their
contracts agree.

A `Serialize` derive on `Commit` must not define the public schema. It would
tie the schema to storage and walk the whole value before the query runs.

### Async operations and serialization

jaq evaluates synchronously, while some jj store and tree operations are async.
The first implementation can use the controlled blocking bridge used by
template extraction. Preserve async boundaries in getters so future prefetching
or awaiting does not require a schema change.

Serialization converts one result into a temporary byte buffer. Only a
successful conversion permits the loop to write that buffer and one line feed.
Extra serialization buffering is limited to one result.

### Delivery order

These are the proposed completion checks. The implementation status above
records where the existing branch has already exposed the interface.

1. Complete an internal phase-zero prototype of core and standard `ValT`
   behavior and the ported v1 built-ins. Verify structural updates, comparison,
   synthetic-module resolution, and rejection of attempts to shadow fallible
   filters. Demonstrate complete buffered serialization and interruption during
   a filter that does not yield. This stage must not expose a public flag.
2. Check in the exact function manifest and jq 1.8.2 conformance tests.
3. Implement the fixed `jj.commit/v1` object and every semantic filter behind
   an internal experimental command.
4. Add slurp, collection tests, and user documentation.
5. Expose `jj log --jq` and query v1 only after compatibility and schema fixtures
   pass.

An internal prototype can change freely but must not identify itself as query
v1. The public contract consists of this document and the
[checked-in function manifest](../../cli/src/query/v1-builtins.toml).

### Testing

Run every jq manifest entry through jq 1.8.2 and jj's engine. Compare values,
errors, and result counts. Cover construction, updates, optional access,
reductions, numeric edge cases, and Unicode. Reject each excluded syntax family,
function, and arity. Use repository fixtures for all synthetic `jj` exports.
Keep fixtures for caught jq errors separate from `jj.query-error/v1` objects.

Instrument getters to verify demand and caching of values and errors.
`keys`, `keys_unsorted`, `has`, and `length` must compute no field values.
Comparison, serialization, and updates must compute only core fields.
Short-circuit tests must show that skipped branches do no expensive work.

CLI snapshots must cover every JSON result type and zero, one, or many results.
Verify selection, flat order, limits, reversal, paths, slurp, stable full IDs,
and raw timestamp/offset pairs. Exercise parse errors, runtime errors, values
outside the JSON domain, and broken pipes.

## Background and alternatives

### What templates offered in jj 0.43.0

Several commands accepted `-T`/`--template`. Templates used a statically typed,
functional formatting language. Its `Commit` methods provided bookmarks,
immutable state, signatures, and diffs. The renderer evaluated each property
when it needed the value.

[PR #6777][pr-6777] implemented the global `json(value)` template function after
[issue #5648][issue-5648]. It enabled the following command.

```console
$ jj log --no-graph -T 'json(self) ++ "\n"'
{"commit_id":"...","parents":["..."],"change_id":"...",...}
```

Programs had to suppress the graph and add a record separator. Without the
newline, adjacent JSON values were concatenated. `json(self)` exposed the
backend representation without a compatibility guarantee and omitted parts of
the semantic `Commit` interface. Constructing objects required string
concatenation and escaping because templates had no general objects with mixed
value types.

Callers also had to know whether a command evaluated its template once, per
commit, or per diff entry. Templates could map homogeneous lists, filter,
branch, and transform strings. General mixed-type objects, dynamic values,
grouping, and jq-style stream transformations needed another tool and an
intermediate schema.

### jj proposals

These upstream discussions informed the proposal.

[Issue #5662][issue-5662] proposed fixed `--json` output for all commands.
[Issue #3814][issue-3814] discussed composing commands with a shared language
that could defer work until needed. A [comment][issue-3814-json-comment]
described missing record separators and semantic fields in `json(self)`.
[Issue #3219][issue-3219] proposed a broader structured API and identified the
risk of turning serialized internal structures into a public contract.

[Issue #3262][issue-3262] tracked template design. It, map-type
[issue #7697][issue-7697], and [PR #6869][pr-6869] and
[PR #8507][pr-8507] explored JSON object construction. Mixed-type objects would
change template typing and evaluation. Merged [PR #8895][pr-8895] allowed
serialization of homogeneous list maps and conditional results without settling
that design.

[PR #6838][pr-6838] proposed an `ndjson()` alias and asked which layer
should add record separators. [Issue #8738][issue-8738] and
[issue #8407][issue-8407] discussed per-item versus collection inputs.
[PR #9369][pr-9369] attempted the latter and closed without merging. This RFC
assigns separators to the command loop and offers per-item and slurped inputs.
None of those proposals embedded jq or jaq.

### Fixed JSON or external jq

Fixed `--json` output is simpler and works with any JSON tool. The
[GitHub CLI][gh-formatting] combines `--json <fields>` and `--jq <filter>`.
Both interfaces could complement this proposal. Used alone, they repeat field
selection or require another step for renaming, filtering, flattening, or
one-to-many output. A jq filter selects data and shapes output together, with
expensive work visible in explicit `jj::` calls.

Piping `json(self)` into external jq remains a useful fallback for backend
fields. The caller must suppress the graph and add separators. The backend
schema lacks computed semantic fields and is not the proposed public schema.
The extra program must be installed, and jj must serialize the intermediate
value before jq discards unused fields.

### Extend templates

Object and array literals would improve templates independently of this RFC.
They avoid a new dependency and retain static type checks. They do not define
filtering, reductions, dynamic values, updates, aggregate inputs, error handling,
or record separators. Adding all of those would create another data language.
jq already defines them and has an embeddable Rust implementation.

### Interpreter choice

[jq][jq-manual] runs a filter independently for each input and permits zero,
one, or many results. [jaq][jaq] follows much of jq's behavior and supports custom
value traits. It can read jj fields without first converting an entire commit
to `serde_json::Value`. Its result stream fits `jj log`.

jaq does not implement all jq features or prevent resource exhaustion. The
[jaq manual][jaq-manual] describes known differences. This RFC promises only
its specified subset, with jq 1.8.2 as the reference.

[xq][xq] described itself as under development and recommended jaq at the time
of the RFC's review. The reviewed [jq-rs][jq-rs] binding wrapped C libjq 1.6 and mainly
accepted and returned strings. It supplied that library's exact behavior, while
this RFC targets jq 1.8.2. A libjq approach would require eager serialization or substantial
custom C integration for host values, plus a system or bundled C dependency.

A new jq-inspired language could closely fit jj's types. Differences in nulls,
result counts, iteration, and errors would still surprise users of familiar
syntax. jj would have to maintain its parser, evaluator, library, and docs.

### Eager objects and a general API

Eagerly converting the core object to JSON would be cheap and would simplify
integration. It would lose the commit identity that semantic filters need when
values are cloned, slurped, or updated. Restoring it with a hidden token or side
table could give projected ordinary objects unexpected capabilities. A host
value retains explicit identity until an operation requires an ordinary object.

An [Amazon Ion integration][ion-jaq] also uses jaq's custom-value interface for
non-JSON data. The maintainer's [lazy-input discussion][jaq-lazy-input]
distinguishes streamed inputs from slurped arrays. Random access and sharing
make a fully lazy slurped array impractical. This design streams by default
and collects handles for slurp while retaining lazy fields.

A stable RPC API could serve long-running clients. It would also need decisions
about transport, lifecycle, subscriptions, and mutations. The schema proposed
here can inform that later work without defining it now.

## Open questions

- Should the public flag be `--jq` or `--query`? The local implementation uses
  the proposed `--jq`, which names the language. `--query` leaves room for
  other implementations. The naming choice remains a design question.
- Should custom-value functions be generalized upstream or ported into jj?
  Either must meet the same behavior and tests. Upstream support would reduce
  maintenance.
- What is the least costly way for jaq to check for an interrupt during evaluation?
- Should later versions provide raw string output? It must require explicit
  selection and preserve the default JSON Lines contract.

## Future possibilities

Other commands could use the same model after the engine and schema rules are
established.

- `jj evolog --jq` could use an evolution-entry object.
- `jj op log --jq` could use an operation object.
- `jj bookmark list --jq` and `jj workspace list --jq` could expose their items.
- `jj diff --jq` could use one top-level diff object instead of per-entry
  templates.

Further work could add content-based copy detection with stable similarity
rules, or explicit bookmark tracking counts with defined estimates. A fixed
`--json` shorthand could return the stable input object. Query files, aliases,
`--arg` values, and more namespaced filters could follow in a new query version.
Regex filters need a fixed dialect, and `implode` needs to preserve Unicode.

Other possibilities include streaming reductions without a slurped array,
schema introspection for agents and completion, cost metadata or work limits,
and editor or RPC APIs that reuse the semantic schema. Commands should share
language and record-separator rules while defining their own stable inputs.
They must not expose an internal type by serialization alone.

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
