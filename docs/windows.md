# Working on Windows

Jujutsu works the same on all platforms, but there are some caveats that Windows
users should be aware of.

## Line endings conversion

Git-backed workspaces honor the `text`, `eol`, and legacy `crlf` rules in
`.gitattributes` when saving files and checking out commits. For example,
`*.cmd text eol=crlf` stores command scripts with LF and checks them out with
CRLF. Importing a Git checkout with these rules does not create line-ending-only
changes.

Use `-text` or the built-in `binary` attribute macro to preserve a file's bytes.
Use `text=auto` to let Git's binary detection rules decide whether to convert
line endings. This also preserves CRLF in files that already have CRLF in their
stored contents.

For files without line-ending attributes, Jujutsu uses
[`working-copy.eol-conversion`](config.md#eol-conversion-setting). This setting
is similar to Git's [`core.autocrlf`][git-autocrlf][^1]. Jujutsu does not read
`core.autocrlf`, so keep the two settings consistent when you use both tools.
For example, use `working-copy.eol-conversion = "input"` with
`core.autocrlf = input` to store text with LF without converting LF to CRLF on
checkout. These global settings use [binary file detection](config.md#binary-file-detection)
to avoid converting binary files.

Line-ending support does not run Git's external clean or smudge filters.

[^1]: This poses the question of whether we should support reading the
      `core.autocrlf` setting in colocated workspaces. See details at the
      [issue][read-git-config-issue].

[git-autocrlf]: https://git-scm.com/book/en/v2/Customizing-Git-Git-Configuration#_core_autocrlf
[read-git-config-issue]: https://github.com/jj-vcs/jj/issues/4048

## Pagination

On Windows, `jj` will use its integrated pager called `streampager` by default,
unless the config `ui.pager` is explicitly set. See the [pager section of the
config docs](config.md#pager) for more details.

If the built-in pager doesn't meet your needs and you have Git installed, you
can switch to using Git's pager as follows:

```powershell
PS> jj config set --user ui.pager '["C:\\Program Files\\Git\\usr\\bin\\less.exe", "-FRX"]'
PS> jj config set --user ui.paginate auto
```

## Typing `@` in PowerShell

PowerShell uses `@` as part the [array sub-expression operator][array-op], so it
often needs to be escaped or quoted in commands:

```powershell
PS> jj log -r `@
PS> jj log -r '@'
```

One solution is to create a revset alias. For example, to make `HEAD` an alias
for `@`:

```powershell
PS> jj config set --user revset-aliases.HEAD '@'
PS> jj log -r HEAD
```

## WSL sets the execute bit on all files

When viewing a Windows drive from WSL (via _/mnt/c_ or a similar path), Windows
exposes all files with the execute bit set. Since Jujutsu automatically records
changes to the working copy, this sets the execute bit on all files committed in
your repository.

If you only need to access the repository in WSL, the best solution is to clone
the repository in the Linux file system (for example, in
`~/my-repo`).

If you need to use the repository in both WSL and Windows, one solution is to
create a workspace in the Linux file system:

```powershell
PS> jj workspace add --name wsl ~/my-repo
```

Then only use the `~/my-repo` workspace from Linux.

[array-op]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_arrays?view=powershell-7.4#the-array-sub-expression-operator

## Symbolic link support

`jj` supports symlinks on Windows only when they are enabled by the operating
system. This requires Windows 10 version 14972 or higher, as well as Developer
Mode. If those conditions are not satisfied, `jj` will materialize symlinks as
ordinary files.

For colocated workspaces, Git support must also be enabled using the
`git config` option `core.symlinks=true`.
