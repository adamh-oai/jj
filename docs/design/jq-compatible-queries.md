# jq-compatible structured queries

Author: [Adam Hupp](mailto:adamh@openai.com)

**Summary:** Add a `--jq` output mode to commands such as `jj log`. The mode
applies a jq-compatible filter to a stable object for each result. It writes
the results as JSON Lines. Each input object computes a field when the filter
first reads it and caches the result. A filter that selects a few fields
therefore avoids serializing all commit data. The first implementation will
embed the Rust `jaq` interpreter. It will support a documented subset of jq
without calling an external program or exposing jj's storage structs.

## Objective

Make jj output easy for programs and agents to read and process. Keep the
flexibility that makes templates useful.

For example, an agent should be able to request only the data it needs:

```console
$ jj log -r 'ancestors(@, 2)' --jq \
    '{change_id, commit_id, description: (.description | split("\n")[0])}'
{"change_id":"...","commit_id":"...","description":"query design"}
{"change_id":"...","commit_id":"...","description":"previous change"}
```

Each output line is one complete JSON value. Output has no graph characters,
labels, ANSI escapes, or paging. Users do not need to quote JSON in a template,
add a newline, or send a large fixed object to another process.

## Current state as of jj 0.43.0

Several jj commands accept `-T`/`--template`. Templates use a statically typed,
functional formatting language. Its `Commit` type provides methods for
bookmarks, immutable state, signatures, and diffs. The renderer evaluates a
template property only when it needs the property's value.

The global `json(value)` template function serializes a value. [PR
#6777][pr-6777] implemented it after the proposal in [issue #5648][issue-5648].
It supports this command today:

```console
$ jj log --no-graph -T 'json(self) ++ "\n"'
{"commit_id":"...","parents":["..."],"change_id":"...",...}
```

Programs that consume this output must handle several problems:

- Without `--no-graph`, `jj log` prefixes each value with graph characters.
- `json()` does not add a record separator. Omitting `++ "\n"` concatenates
  adjacent JSON values.
- `json(self)` serializes jj's backend commit representation. It does not expose
  the complete semantic `Commit` template interface, and its compatibility is
  not guaranteed.
- Users must concatenate and escape strings to construct a JSON object.
  The template language does not currently support general objects with
  values of different types.
- Each command decides how often to evaluate a template. Callers must know
  whether it runs once, once per commit, or once per diff entry.
- Templates support maps over lists of one value type. They also support
  filters, conditionals, and string transformations. Other operations need
  another tool and an intermediate schema. These include general objects
  with mixed value types, dynamic values, grouping, and jq-style stream
  transformations.

This proposal keeps templates focused on labeled output for people. Programs
need different defaults: a versioned data contract, clear record separators,
and operations for selecting and transforming data.

## Prior work

### jj proposals

[Issue #5662][issue-5662] proposes fixed `--json` output for all commands.
[Issue #3814][issue-3814] discusses how to combine commands. It notes that a
shared language could let commands defer work until a value is needed. A
[recent comment][issue-3814-json-comment] describes missing record separators
and semantic fields in `json(self)`. [Issue #3219][issue-3219] proposes a
broader structured API. It also identifies a compatibility risk: serializing
internal structures makes implementation details part of a public API.

[Issue #3262][issue-3262] tracks the template design. That discussion, open
map-type [issue #7697][issue-7697], and open [PR #6869][pr-6869] and [PR
#8507][pr-8507] explore JSON object construction. Objects that contain
different value types would change the template language's type and evaluation
model. Merged [PR #8895][pr-8895] allows serialization of homogeneous list maps
and conditional results. It does not settle the design for objects that contain
different value types.

Open [PR #6838][pr-6838] proposes an `ndjson()` template alias. Its discussion
asks which layer should add record separators: the serializer or the loop that
emits results. This RFC assigns that work to the loop.

[Issue #8738][issue-8738] and [issue #8407][issue-8407] discuss whether a
template should receive individual items or a collection. An implementation
attempt in [PR #9369][pr-9369] was closed without merging. This design supports
both modes. By default, a filter receives one item at a time. With `--slurp`,
it receives one collection.

None of these proposals embeds jq or jaq.

### jq and other CLIs

[jq][jq-manual] provides a compact language for selecting fields, filtering
values, building objects, and transforming streams. The jq command applies its
filter separately to each input JSON value. Each invocation can emit zero, one,
or many values.

The [GitHub CLI][gh-formatting] has a related interface. Commands select fields
with `--json` and can transform them with `--jq`. jj can avoid building the
full intermediate JSON value because its commit objects and query engine run in
the same process.

### Rust implementations

[jaq][jaq] is a Rust implementation of jq that can run inside another program.
It aims to match jq in most cases. Programs can implement jaq's value traits
for their own types. A host value supplies field indexing and iteration. This
lets jj expose a commit without first converting the whole commit to
`serde_json::Value`. A compiled filter returns a stream of results, which fits
`jj log`.

jaq does not support every jq filter or command-line feature. Its engine also
does not prevent CPU, memory, or stack exhaustion. This design defines the jq
behavior that jj promises to support. It does not promise to accept every
program that a particular jq binary accepts.

Other implementations fit this design less well. [xq][xq] describes itself as
under development and recommends jaq. [jq-rs][jq-rs] wraps C libjq 1.6 and
mainly accepts and returns strings. It matches that older libjq version
exactly. However, it does not directly support a custom lazy jj value, and it
adds a C build or system-library dependency.

## Goals and non-goals

### Goals

- Produce versioned output that programs can read without ambiguity.
- Let users construct JSON objects and arrays with familiar jq syntax.
- Compute only the semantic fields a filter needs, where jq behavior permits.
- Stream selected commits and results without collecting the entire log.
- Keep revsets and filesets as jj's optimized selection languages.
- Specify output order, record separators, failures, and partial output.
- Share semantic field implementations with templates where practical.
- Support common jq projections, filters, and reductions without an external
  jq executable.

### Non-goals

- Replace templates for output intended for people.
- Use jq to change the repository.
- Expose arbitrary Rust or backend storage structures.
- Support every jq command-line option, module, I/O feature, or
  implementation detail.
- Make every jq operation lazy. Sorting, grouping, random access, and array
  construction must retain values.
- Replace revsets with jq. `select()` filters data during projection.
  Revsets use the index to select revisions before projection.
- Define a stable network or editor RPC protocol. [Issue #3219][issue-3219]
  covers that broader scope.
- Safely sandbox untrusted filters.

## Overview

The initial command-line interface is:

```txt
jj log [LOG OPTIONS] --jq <FILTER> [--query-version v1] [--slurp]
```

`jj log` loads the repository, snapshots the working copy, and selects commits
with its existing revset and fileset rules. It applies the existing order and
limit options. It then wraps each selected commit in a `jj.commit/v1` query
value and evaluates the compiled filter. The query version selects both the jq
language contract and the input schema. In v1, these do not have separate
versions.

Without `--slurp`, the filter runs once for each selected commit, as jq does
for multiple inputs. Each invocation can emit zero, one, or many values. jj
writes each value as compact JSON followed by one line feed.

With `--slurp`, jj collects the selected commit handles into an array and runs
the filter once. It still computes commit fields only when needed. It must
retain the array of handles to support random access and reordering, as jq's
slurp mode does.

jj compiles the filter before writing any result. The query engine owns a
read-only view of the repository and a registry of semantic fields. It computes
each core field on first access. It also caches the value or error from each
namespaced semantic filter for that input object.

## Detailed design

### Command-line interface

`--jq <FILTER>` selects structured query output. The first version has no short
option. It cannot be combined with `--template`, `--patch`, diff-format flags,
or `--count`. Those options select or append another output type.

Query mode accepts `--no-graph`, but always omits the graph. It does not
request a pager. It never adds color or formatter labels to standard output,
even with `--color=always`. Diagnostics on standard error follow the normal UI
settings.

Revision and path arguments select commits before the query runs. `--limit`
limits the number of commits, not the number of results. `--reversed` reverses
that limited commit stream. `--ignore-working-copy` keeps its global meaning.
For example, `--limit 10 --jq 'select(.conflict)'` examines at most ten
selected commits. It does not continue until it finds ten outputs. A filter
that emits two values per commit can produce twenty outputs from those ten
commits.

Query mode uses the same order as `jj log --no-graph` with the same revset,
paths, limit, and reversal options. It ignores `revsets.log-graph-prioritize`,
which applies only to graph output. jj still writes warnings to standard error
when path arguments match no entries.

`--query-version <VERSION>` requires `--jq`. The first and default version is
`v1`. Scripts must opt into future language or schema versions. Each query
version fixes the available fields, jq reference version, built-in names and
arities, and host-value behavior.

Each command versions its query contract separately. For `jj log`, v1 combines
the shared query language v1 manifest with the `jj.commit/v1` input schema.
Another command can introduce its own v1 schema without changing `jj log`.
Likewise, `jj log` can add v2 without requiring other commands to add v2.

`--slurp` also requires `--jq`. It passes one array of selected commit objects
to the filter and invokes the filter once. Memory use can grow with the number
of selected commits and the data that the filter computes.

The first version accepts only a command-line filter string. Later work can add
query files, aliases, positional jq arguments, and raw-output flags without
changing the data model.

### Input and result cardinality

The compiled filter runs once per selected commit. Each invocation can emit
zero, one, or many results. jj preserves commit order and the order of results
from each invocation. Empty output is successful. For example:

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

The filter determines how many values it emits. The serializer does not wrap
multiple values in an array. A user who needs one array can construct it in
`--slurp` mode.

### Output framing

jj writes each result as compact JSON followed by `\n`. The output is JSON
Lines. Each line is a separate JSON value. Strings, numbers, booleans, and null
are valid results, and jj encodes them as JSON:

```console
$ jj log -r @ --jq '.description'
"A description with an escaped newline\n"
```

v1 does not support jq's `--raw-output`, `--join-output`, pretty printing, or
color output. Those modes make record boundaries depend on the value type,
which callers can misread. A caller that needs text can decode the JSON string.

jj buffers one result before writing it. If conversion rejects a non-JSON
value, jj has not yet written any of that record. A runtime error can still
occur after earlier records succeed. Callers must check the process exit status
before treating the stream as complete.

The command adds record separators. The filter and the `json` serializer do
not. This resolves the ambiguity in [PR #6838][pr-6838]: a reusable serializer
converts values, while the command separates the records it emits.

### jq compatibility contract

The implementation embeds pinned versions of `jaq-core` and its standard
library. Query v1 uses [jq 1.8.2][jq-1.8.2] as its reference. jj permits only a
fixed set of jq functions. Tests compare their names, arities, values, errors,
and number of results with that release. The exceptions are listed below.

v1 accepts only the following user syntax:

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

jj rejects destructuring bindings, `label`/`break`, module declarations, and
all other unlisted syntax. This rule applies even when the embedded jaq parser
accepts the syntax. The workspace pins `jaq-parse` as well as the evaluator
crates. After parsing, a validator checks the user's abstract syntax tree
(AST). The compiler adds private library definitions only after this check.
Negative conformance tests cover every excluded syntax family. These checks
prevent a dependency upgrade from silently adding syntax to v1.

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

The implementation must check in `cli/src/query/v1-builtins.toml` as the public
manifest that defines the contract. It lists every public jq name and supported
arity, plus every synthetic `jj` export and arity. It contains no
implementation details.

A separate checked-in loader table records whether each entry is native or a
library definition. It also lists the private definitions each entry needs.
This implementation table can change without a new query version if observable
behavior stays the same. The loader follows only those dependencies and a fixed
native-function table. Private helpers must be renamed internally or otherwise
kept unavailable to user queries. Loading every definition from `jaq-std` would
expose excluded functions.

Conformance tests must check every public manifest entry and verify that
excluded names fail to compile. A debug command should print the effective
public manifest and dependency versions for bug reports.

v1 excludes the following features:

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

jj promises compatibility for this subset of jq. The checked-in public manifest
defines the available functions. Conformance tests compare its jq entries with
jq 1.8.2. Schema and behavior fixtures test the synthetic `jj` entries. A newer
jaq dependency must not expand that contract.

Adding or removing a field or public entry requires query v2. Changing an arity
or observable host-value behavior also requires v2. A bug fix that restores the
stated jq behavior does not require a new version.

jj-specific filters use the `jj::` namespace to prevent name collisions with
future jq functions. jaq parses this spelling as a module-qualified call.
During compilation, jj installs a synthetic module named `jj`. The module is
always in scope. It has no source or filesystem search path. Users cannot
import or shadow it. It exports only the namespaced entries in the v1 manifest.
A user definition in the reserved `jj` namespace is a compile error.

### Host values and jq values

Filters construct ordinary JSON-like jq values. Input commits are typed host
values: jj objects that the query engine exposes through the object interface
below. Their core fields have limited evaluation cost.

`.commit_id` reads a field from a host object. `keys` lists its available
fields. `{id: .commit_id}` builds an ordinary object. A missing key returns
`null`, so jq fallbacks work:

```console
$ jj log -r @ --jq '.future_field // "not available"'
"not available"
```

`has("future_field")` returns false. jj may later add an optional strict-field
lint to catch misspellings. In v1, static field access keeps jq's missing-key
behavior.

After jj loads a `Commit`, its advertised fields can be computed without
failure or open-ended repository work. Standard jq object operations treat a
host commit like the ordinary JSON object formed from those fields:

- direct field indexing computes only that field;
- `keys`, `keys_unsorted`, `has`, and `length` use the static field registry;
- `.[]`, `to_entries`, recursive descent, string interpolation,
  `tostring`, `tojson`, comparison, sorting, object addition, and serialization
  compute the complete core object;
- the first assignment, update, or deletion through a host object materializes
  it as an ordinary object and then follows jq's normal missing-key,
  zero-result, and multi-result update behavior.

Computing all core fields cannot call a repository operation that can fail.
This lets jj implement jaq's infallible display, equality, and ordering traits
with ordinary jq object behavior. Explicit `jj::` filters provide repository
computations that are expensive or can fail. They return an ordinary JSON value
or a jq error.

Assignment changes only the query value. For example, `.description =
"replacement"` produces an ordinary detached object. The query environment has
only read access to the repository. A detached object has no commit identity,
so a `jj::` filter rejects it with a type error. A query that needs both forms
can bind the original host value before updating it.

Binding, cloning, slurping, or reordering a host value preserves its commit
identity. Storing it as an element of an ordinary array or object also
preserves that identity. Reading a core field returns the field's ordinary
value. Updating or deleting through the host object converts it to a detached
ordinary object. Object addition and a `tojson | fromjson` round trip also
remove its commit identity.

### Commit schema v1

The input object's `.schema` is `"jj.commit/v1"`. The object exposes the
semantic fields listed below. It does not expose the fields from a Rust
`Serialize` derive. IDs are full-length because the shortest unambiguous ID
depends on other IDs in the repository.

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

`parent_ids` contains IDs. A `parents` field containing commit objects could
make serialization of `.` walk all history. A later design can add an explicit
operation to traverse parent commits.

Identity objects contain `name`, `email`, and `timestamp`, in that order. The
timestamp object contains integer `millis_since_epoch` and `utc_offset_minutes`
fields. This representation preserves every backend timestamp without
converting it to a calendar date, which can fail. Trailer objects contain `key`
and `value` strings in description order. Here, "identity" means author or
committer information, not a cryptographic commit signature.

v1 has a fixed field set. Adding an enumerable field changes `keys`, `.[]`,
comparison, serialization of `.`, execution cost, and possible failures. It
therefore requires query v2, even if a JSON consumer could ignore the new
field. The table above also defines the host object's insertion order. Queries
observe that order through `keys_unsorted`, `.[]`, `to_entries`, and
serialization. `keys` keeps jq's lexicographic order.

Every object supplied by the schema follows this ordering rule. Tables and
inline field lists in this document specify insertion order. Every documented
key is present, with null where specified, unless the whole object is null.

The `jj log` v1 contract fixes the shapes of core objects and namespaced-filter
results. It also fixes nullability, enum strings, object-key order, and
documented array membership and order. Changing any of these requires `jj log`
query v2. `parent_ids` keeps stored parent order. Trailers keep description
order.

### jj semantic filters

Explicit namespaced filters provide data that requires repository queries.
These values are not enumerable core fields. Evaluating `.` therefore stays
within the core object's limited work. Each filter runs only when needed. Every
filter below expects a `jj.commit/v1` input and returns an ordinary JSON value
or a jq error:

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

Namespaced filters follow normal jq control flow. A filter in a branch that jq
does not select does not run:

```jq
if jj::signature_present then jj::verify_signature else null end
```

jj caches each filter's result for each commit. Calling `jj::immutable` twice
evaluates the policy once. Filters that use the same index or
signature-verification result share that work.

`jj::mine` compares the commit author email with the configured user email
using exact string equality. `jj::change_offset` returns the non-negative
[change offset](../glossary.md#change-offset). Zero identifies the most recent
visible commit for that change ID. Larger integers identify successively older
versions. The result is null when the input commit cannot be assigned an offset
in the visible change.

`jj::working_copies` includes every workspace whose working-copy commit is the
input commit. The array is sorted lexicographically by workspace name.

Bookmark and tag arrays include every local or remote reference whose normal,
removed, or added target terms include the input commit. They keep references
that some templates remove as display duplicates. The arrays are sorted
lexicographically by `(name, remote)`. A local reference's null remote sorts
before named remotes. Removed and added target ID arrays are sorted
lexicographically by full commit ID. Here and below, lexicographic string order
means Unicode scalar-value order.

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

`present` is false only when the target is resolved and absent. `conflict` is
true when the target is unresolved. A resolved normal target has its ID in
`normal_target_id` and empty removed and added arrays. A resolved absent target
has a null normal ID and empty arrays. An unresolved target has a null normal
ID. Its removed array contains the simplified, non-null negative merge terms.
Its added array contains the corresponding positive terms.

For a local reference, `tracked` and `tracking_present` are false. `synced` is
true when the local target equals every tracked remote target. It is also true
when there are no tracked remote targets.

For a remote reference, `tracked` reports its tracking state.
`tracking_present` requires both tracking and a present local target. `synced`
requires tracking and exact target equality. Equality includes the structure of
conflicted and absent targets. An untracked remote therefore has all three
tracking booleans set to false.

Fields that do not apply use `false` or `null`, as specified, and keep their
declared type. The object omits ahead and behind counts. Returning those counts
in an ordinary object would require both graph walks, even when a query selects
only the reference name. A future explicit filter can expose counts and specify
whether they are exact or estimated.

`jj::verify_signature` returns null for an unsigned commit. For a signed commit
it returns:

| Field | JSON type | Meaning |
| --- | --- | --- |
| `status` | string | `good`, `bad`, `unknown`, or `invalid` |
| `key` | string or null | Backend key identifier, if available |
| `display` | string or null | Backend-specific signer display text, if available |

An invalid-format result has status `invalid`, with null `key` and `display`
fields. Other backend failures produce jq errors.

Use `jj::signature_present` when only signature presence is needed. It does not
run GPG, `ssh-keygen`, or other verification work.

`jj::diff_files` describes structural changes from the merged parents to this
commit. It honors the path arguments passed to `jj log`. It does not load file
contents. Each result contains `path`, `status`, `source`, and `target`, in
that order. `status` is `modified`, `added`, or `removed`. v1 does not detect
copies or renames from file contents.

Each non-null side contains `path`, `file_type`, `executable`, `conflict`, and
`conflict_side_count`, in that order. An absent side is null. For a present
side, `file_type` is `file`, `symlink`, `tree`, `git-submodule`, or `conflict`.
`executable` is a boolean for a resolved `file` and null for every other type.
`conflict` is true exactly when the simplified entry merge is unresolved. In
that case, `file_type` is `conflict`. `conflict_side_count` counts positive
terms after simplifying the merge. It is one for a resolved entry.

The top-level `path` is the target path when present, or the source path
otherwise. Results are sorted by target path, then source path, then status.
Sorting uses lexicographic repository-path order. An absent path sorts before a
present path.

`jj::diff_stats` calculates line diffstats from the merged parents to this
commit. It honors the path arguments passed to `jj log`. It reads the
materialized contents of changed files only when the filter runs. The result
contains `files`, `total_added`, and `total_removed`, in that order. `files` is
sorted by repository path. Each file object has these fields:

| Field | JSON type | Meaning |
| --- | --- | --- |
| `path` | string | Target path, or source path for a removed file |
| `status` | string | `modified`, `added`, or `removed` |
| `lines_added` | integer or null | Added line count, or null for binary content |
| `lines_removed` | integer or null | Removed line count, or null for binary content |
| `bytes_delta` | integer | Target byte size minus source byte size |

`total_added` and `total_removed` sum only the per-file line counts that are
not null. Binary files do not contribute to either total. As with
`jj::diff_files`, v1 does not detect copies or renames from content. File
object keys appear in this order: `path`, `status`, `lines_added`,
`lines_removed`, `bytes_delta`.

### Evaluation and materialization

Evaluation is lazy in three ways:

1. The revset produces commit IDs as a stream in normal mode.
2. The filter produces zero or more results as a stream.
3. A direct field or namespaced semantic filter runs only when jq evaluates
   that expression.

The third property lets a query avoid computing unused data. For example:

```jq
{change_id, author: .author.email}
```

jj loads only the change ID and author identity metadata for this query. It
does not enumerate bookmarks, evaluate immutability, compare trees, verify
signatures, or calculate a tree diff.

Each host object owns a shared cache for its commit. The cache stores direct
field values and the value or error of each `jj::` filter that runs. Reading
`jj::empty` twice compares the trees once. `jj::signature_present` and
`jj::verify_signature` share loaded signature metadata. Clones of a host value
share its cache.

In normal mode, jj releases the cache after writing all results for that input
commit. In `--slurp` mode, retained host values keep their caches.

Lazy evaluation must preserve jq behavior. The following operations can cause
jj to compute or retain values:

- Serializing `.` computes all core commit fields. It never invokes a `jj::`
  filter.
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

Objects and arrays constructed by a jq program are ordinary jaq values. This
RFC does not require those collections to be lazy.

### Repository consistency

All host objects in one invocation share the `ReadonlyRepo` created during
command initialization. Evaluating a field later must not make it observe a
later operation or a different working-copy snapshot.

Before selecting commits, jj snapshots the working copy under the existing
command rules. Query mode changes output only. It adds no repository mutation.
With `--ignore-working-copy`, it observes the same state as ordinary `jj log
--ignore-working-copy`.

The query environment owns repository indexes that can be reused across
commits. It also owns backend caches with their own size limits. It must not
retain every per-commit query result. Lazy evaluation controls when a cache is
filled. All cached values still refer to the same repository state.

### Errors and exit status

jj parses and compiles the filter before evaluating the first commit. A syntax
error reports its source span on standard error. The command then exits nonzero
without writing standard output.

A missing object key returns null, as in jq. After the commit loads, core field
access cannot fail. A namespaced filter can fail with a runtime query error.

A pure jq runtime diagnostic identifies the query and error. When a `jj::`
filter fails on a host commit, the diagnostic also identifies the input commit
ID and underlying jj error. A pure jq error need not have an associated commit.
Neither does a `jj::` type error on a detached value. jaq does not attach a
source span to every runtime error. v1 therefore promises an exact span only
for parse and compile errors.

Queries can observe the value of a caught error. Pure jq operations produce the
same caught value as jq 1.8.2. A jj-defined error produces this ordinary
object, with keys in the order shown:

```json
{"schema":"jj.query-error/v1","kind":"host","filter":"jj::empty","commit_id":"..."}
```

`kind` identifies the error category. It is `host` for a repository,
configuration, or backend failure. It is `type` when a `jj::` filter receives a
non-host value. It is `non-finite` for the documented numeric deviation.

All four keys are present. A `host` error has string `filter` and `commit_id`
values. A `type` error has a string `filter` and null `commit_id`. A
`non-finite` error has null for both fields. The caught object omits
human-readable source text because that text can change. If the query does not
catch the error, the diagnostic on standard error includes the underlying jj
error chain. `try`, `catch`, and `?` handle these errors normally.

An unhandled runtime error stops evaluation and makes the command exit nonzero.
Earlier lines remain valid JSON, but the stream is incomplete. Structured
output does not insert the `<Error: ...>` strings that templates can render.

If a write fails because the pipe is closed, jj stops evaluation without
computing more fields. It follows the normal jj broken-pipe exit behavior.
Revset-stream failures and failures to load a selected commit are command
errors outside the filter. They can occur after jj writes earlier records.

A query cannot catch cancellation, interruption, these command errors, broken
pipes, or internal invariant failures. They terminate the host command.

### Resource behavior

A normal per-commit projection retains one input, query state, shared
repository caches, and one buffered output value. A single result can still be
arbitrarily large.

jaq does not prevent resource exhaustion. Filters can recurse, construct large
values, or do expensive work across many commits. v1 treats filters as trusted
local input, like revsets and templates. It must stop promptly when the output
pipe closes. It does not enforce hard CPU or memory quotas.

Users must be able to interrupt the public query mode. Before exposing it, a
phase-zero prototype must show that jj checks for interruption during a long
pure filter. Checking only between results is insufficient. If jaq lacks a
suitable hook, jj must contribute one upstream or carry a small integration
patch before enabling `--jq`.

Commands that accept filters from a remote or untrusted source must supply
their own policy and resource limits. A later design may add configurable
limits on execution, recursion, materialization, or output. Those limits should
not silently truncate valid JSON Lines.

### Encoding

JSON strings are Unicode. v1 exposes only values that jj can represent as
Unicode strings. Repository paths use slash-separated repository syntax rather
than platform display syntax. v1 excludes full file contents and arbitrary
backend byte strings.

Every output value must be representable as JSON. Object keys must be strings,
and numbers must be finite. A commit host value that the query has not
projected serializes as its core object.

jj buffers each record before writing it. If a value cannot be represented, jj
raises a runtime error without writing a partial line. The v1 manifest excludes
named non-finite constructors. `QueryValue` rejects non-finite results from all
other numeric operations. The manifest also excludes filters that decode
arbitrary bytes.

If a later version adds byte-valued fields, it must preserve invalid UTF-8
bytes. Such a field should use an explicit base64 string, or a new query
version must define the conversion of byte strings.

## Implementation plan

### Query engine

Add pinned `jaq-core` and `jaq-std` workspace dependencies. Use `jaq-json` as a
reference where useful. Its native functions require `jaq_json::Val`, so they
cannot be loaded directly for a custom value.

Compile filters with the checked-in v1 manifest. Add the synthetic `jj` module
without enabling jaq's user module loader. If jaq's public API cannot do this,
contribute a small compiler extension upstream or maintain it in jj. Do not run
`jq` or `jaq` as a subprocess.

The CLI query module should contain three layers:

1. Parsing, compilation, compatibility filtering, and diagnostics.
2. A `QueryValue` implementation of both [`jaq_core::ValT`][jaq-core-valt]
   and [`jaq_std::ValT`][jaq-std-valt].
3. Command-specific adapters such as `CommitQueryObject`.

`QueryValue` contains ordinary scalar, array, and object values, plus commit
host objects with the core fields defined above. `ValT::index()` resolves only
the requested key. `values()` and `key_values()` compute each core field they
yield, as jq object behavior requires.

These iterator APIs can propagate `ValR` errors. However, v1 core getters must
be infallible because display, equality, and ordering cannot return errors.
jj's implementations of `keys`, `keys_unsorted`, `has`, and `length` consult
the static registry without computing field values.

`jaq_std::ValT` also requires ordering, numeric conversion, sequence
operations, and byte and string operations. The [`jaq-json`
functions][jaq-json-funs] for `jaq_json::Val` include `length`, `has`,
`contains`, `fromjson`, and `tojson`. These functions must be generalized
upstream or ported to `QueryValue`. The checked-in manifest defines which
functions need this work and tests.

The child iterators from `ValT::values()` and `key_values()` return `'static`
values. This makes direct borrowing from the current
`CommitTemplateLanguage<'repo>` impractical. Instead, the query environment
owns an `Arc<ReadonlyRepo>`, cloned commits, path conversion state, field
registries, and shared caches. This ownership also gives all values the same
repository snapshot. It is a jj integration choice. jaq does not require every
value to own its data.

jaq requires host-value display, equality, and ordering to be infallible. These
operations compute the core object and apply ordinary jq structural behavior.
Core fields have limited evaluation cost and cannot fail. Namespaced filters
return errors directly from their fallible calls.

### Sharing semantic fields

The commit template method table already defines most of the needed getters.
Template properties already provide a lazy, fallible `extract()` interface. The
proposed shared registry adds cost metadata.

Extract a common semantic getter layer for bookmark, immutability, diff, and
signature behavior. Share getters only where the contracts match. For example,
v1 diff filters honor command-line paths. The current commit template `diff()`
defaults to all paths.

Existing template objects borrow repository state. `QueryValue` owns it to meet
the child iterators' lifetime requirements. Directly wrapping the current boxed
template properties is therefore unlikely to work. A shared registry can
instead describe:

- the versioned field or filter name and result type;
- its cost category and documentation;
- an owned getter accepting a query environment and host value;
- adapters into template methods and query fields or filters where contracts
  are the same.

Do not derive `Serialize` on `Commit` to implement the query schema. That would
tie the public schema to backend storage and walk the whole value before the
query runs.

### Async operations

jaq evaluates filters synchronously. Some jj store and tree operations are
async. The first implementation can use the controlled blocking bridge used by
current template field extraction. Keep the getters' async boundaries so a
future engine can prefetch or await fields without changing the schema.

### Serialization

Convert or serialize one result into a temporary byte buffer. Write that buffer
and `\n` to standard output only after conversion succeeds. If conversion finds
a non-JSON value, no part of the record has been written. Extra buffering is
limited to one result.

### Delivery order

1. Build a phase-zero prototype without a public flag. It must demonstrate
   the core and standard `ValT` implementations and the ported v1 built-ins.
   Test structural host updates and comparison. Check synthetic-module
   resolution and rejection of attempts to shadow fallible namespaced filters.
   Demonstrate complete buffered serialization. Also demonstrate interruption
   during a filter that does not yield results.
2. Check in the jq 1.8.2 conformance tests and exact v1 function manifest.
3. Implement the fixed `jj.commit/v1` core object and every v1 namespaced
   filter. Keep them behind an internal experimental command.
4. Add `--slurp`, collection tests, and user documentation.
5. Expose `jj log --jq` and query version v1 only after all compatibility and
   schema fixtures pass.

An internal prototype can change freely, but it must not identify itself as
query v1. The public v1 contract consists of this document and the exact
`cli/src/query/v1-builtins.toml` manifest. Check in that manifest before
shipping the public flag.

### Testing

Run the jq entries in the v1 manifest through jq 1.8.2 and jj's engine. Compare
both values and errors. Use repository fixtures for synthetic `jj` exports.
Cover construction, result counts, updates, optional access, reductions,
numeric edge cases, and Unicode. Negative tests should cover every excluded
syntax family, function, and arity.

Use separate fixtures for caught jq-compatible values and `jj.query-error/v1`
objects. Instrument getters to count calls. Verify that direct fields run only
when needed and cache their results. Check cached filter values and errors.
Check that `keys`, `keys_unsorted`, `has`, and `length` do not compute field
values. Verify that comparison, serialization, and updates compute only core
fields. Test that short-circuit evaluation skips expensive filters.

CLI snapshots should cover valid JSON Lines for every result type and number of
results. Cover selection, flat ordering, limits, reversal, paths, and slurp.
Test parse errors, runtime errors, values outside the JSON domain, and broken
pipes. Check stable full IDs and raw timestamp/offset pairs.

## Alternatives considered

### Fixed JSON and two-stage field selection

The fixed schema proposed in [issue #5662][issue-5662] is simpler and works
with any JSON tool. The GitHub CLI's `--json <fields> --jq <filter>` also makes
field costs explicit. Either could complement this proposal.

Used alone, these approaches repeat field selection or require another step to
rename, filter, or flatten values. They also need another step for one-to-many
output. A jq filter selects core fields and defines the output shape together.
Expensive `jj::` filters remain explicit calls.

### Extend templates with object and array literals

[PR #6869][pr-6869] and [PR #8507][pr-8507] explore adding object and array
literals to templates. This avoids a new dependency and preserves static
template type checking. These literals would improve templates independently of
this RFC.

Object construction does not define filtering, reductions, dynamic values,
updates, aggregate inputs, error handling, or record separators. Adding all of
these would create another data language for users and maintainers to learn. jq
already defines this behavior and has a Rust implementation that jj can embed.

### Pipe `json(self)` to external jq

This works today for backend commit fields when callers suppress the graph and
add record separators. It is a useful fallback. However, the backend schema
lacks computed semantic fields and is not the proposed public schema. The
approach also requires another installed program and process. jj must serialize
the intermediate value before jq can discard unused fields.

### Embed C libjq or define a new language

libjq provides the exact behavior of the embedded library version. The
available Rust binding targets libjq 1.6, while this RFC uses jq 1.8.2 as its
reference. The binding accepts strings. jj would therefore need to serialize
inputs in advance or build a substantial C integration for host values. It
would also need a native C dependency, either from the system or a bundled
build. jaq's custom-value interface better supports semantic filters that run
only when needed.

A new jq-inspired language could closely match jj's types. But small
differences in null handling, result counts, iteration, and errors would
surprise users of familiar-looking filters. jj would also have to maintain a
new parser, evaluator, standard library, and documentation.

### Eager JSON and a general API

Converting the core object to JSON in advance would be inexpensive and would
simplify interpreter integration. However, it would lose the typed commit
identity needed by `jj::bookmarks`, `jj::empty`, and other semantic filters.
That identity matters when jq values are cloned, slurped, or updated. A hidden
side table or token could restore identity, but would give projected ordinary
objects unexpected capabilities. A custom host value retains explicit commit
identity until a jq operation requires an ordinary object.

A stable RPC API would better serve long-running clients. It would also need
decisions about transport, lifecycle, subscriptions, and mutations. The query
schema can inform such an API later.

## Open questions

- Should the public flag be `--jq` or `--query`? `--jq` names the language.
  `--query` leaves room for other implementations. This RFC proposes `--jq`
  because it tells users what to expect.
- Should jaq support custom-value functions upstream, or should jj port them?
  Both choices must meet the same behavior and conformance requirements.
  Upstream support would reduce long-term maintenance.
- What is the smallest jaq interruption hook that can check jj's cancellation
  state without adding significant overhead to every filter step?
- Should later versions support raw string output? Users must explicitly
  select that mode. It must not weaken the default JSON Lines contract.

## Related work

The [jq manual][jq-manual] defines the behavior that users will expect. [jaq's
manual][jaq-manual] documents known differences from jq. Other programs already
use jaq's custom-value interface to expose non-JSON data. An [Amazon Ion
integration][ion-jaq] uses an approach close to the semantic values proposed
here.

The jaq maintainer's [discussion of lazy inputs][jaq-lazy-input] distinguishes
two cases. jaq supports streaming independent inputs. A fully lazy slurped
array is impractical because it must support random access and sharing. This
RFC therefore streams inputs by default. `--slurp` collects commit handles but
still computes their core fields only when needed.

The GitHub CLI's [`--json` and `--jq` interface][gh-formatting] shows how users
can apply jq without installing another executable. This RFC puts field
selection inside a lazy host object. It also defines JSON Lines record
separators explicitly.

## Future possibilities

After the query engine and schema rules are established, other commands could
use the same model:

- `jj evolog --jq` with a versioned evolution-entry object;
- `jj op log --jq` with an operation object;
- `jj bookmark list --jq` and `jj workspace list --jq`;
- `jj diff --jq` with one top-level diff object instead of per-entry templates;
- copy detection that reads file contents and defines stable similarity rules;
- explicit bookmark tracking-count filters with stable rules for estimates;
- a fixed `--json` shorthand that returns the input using a stable schema;
- query files, aliases, `--arg` values, and additional namespaced filters in a
  new query version;
- regular-expression filters with a fixed dialect and an `implode`
  implementation that preserves valid Unicode;
- streaming reductions over all inputs without constructing a slurped array;
- a command that shows the schema to agents and completion systems;
- cost metadata or work limits for expensive fields;
- editor and RPC APIs which reuse the same semantic schema.

These commands should share the query language and record-separator rules. Each
command must define its own stable input schema instead of serializing an
internal type.

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
