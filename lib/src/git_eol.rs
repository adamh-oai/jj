// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Git's per-file line-ending rules, without external filters or index writes.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use blake2::Blake2b512;
use blake2::Digest as _;
use bstr::ByteSlice as _;
use futures::AsyncRead;
use futures::AsyncReadExt as _;
use futures::io::Cursor;
use gix::attrs::StateRef;
use gix::attrs::glob::pattern::Case;
use gix::attrs::search::MetadataCollection;
use gix::attrs::search::Outcome;
use gix::filter::plumbing::eol;
use gix::filter::plumbing::eol::AttributesDigest;

use crate::backend::TreeValue;
use crate::eol::EolConversionMode;
use crate::eol::EolSource;
use crate::git::get_git_backend;
use crate::matchers::Matcher as _;
use crate::matchers::PrefixMatcher;
use crate::merged_tree::MergedTree;
use crate::repo_path::RepoPath;
use crate::repo_path::RepoPathBuf;
use crate::repo_path::RepoPathComponent;
use crate::store::Store;

#[derive(Clone, Default)]
struct DirectoryRules {
    inherited: gix::attrs::Search,
    info: gix::attrs::Search,
    collection: MetadataCollection,
    changed: bool,
    contents: Option<Vec<u8>>,
}

pub(crate) struct GitEol {
    store: Arc<Store>,
    tree: MergedTree,
    root: PathBuf,
    source: EolSource,
    case: Case,
    config: eol::Configuration,
    globals: DirectoryRules,
    info_contents: Option<Vec<u8>>,
    directories: Mutex<HashMap<RepoPathBuf, Arc<DirectoryRules>>>,
    external_fingerprint: Vec<u8>,
    sparse_matcher: PrefixMatcher,
}

impl GitEol {
    pub(crate) fn new(
        store: Arc<Store>,
        tree: MergedTree,
        root: PathBuf,
        mode: EolConversionMode,
        source: EolSource,
    ) -> io::Result<Option<Self>> {
        let Ok(backend) = get_git_backend(&store) else {
            return Ok(None);
        };
        let mut repo = backend.git_repo();
        if root.join(".git").try_exists()? {
            // Linked worktrees can have their own config.worktree. Read it
            // without loading or writing the index.
            let worktree_repo =
                gix::open_opts(&root, repo.open_options().clone().open_path_as_is(false))
                    .map_err(other)?;
            if dunce::canonicalize(repo.common_dir())?
                != dunce::canonicalize(worktree_repo.common_dir())?
            {
                return Err(io::Error::other(
                    "Working-copy Git repository differs from its JJ backend",
                ));
            }
            repo = worktree_repo;
        }
        let settings = repo.config_snapshot();
        let permissions = repo.open_options().permissions.attributes;
        let environment = repo.open_options().permissions.env;
        let mut attribute_env = |name: &str| {
            let permission = match name {
                "HOME" => {
                    return environment
                        .home
                        .is_allowed()
                        .then(gix::path::env::home_dir)
                        .flatten()
                        .map(Into::into);
                }
                "XDG_CONFIG_HOME" => environment.xdg_config_home,
                name if name.starts_with("GIT_") => environment.git_prefix,
                _ => return None,
            };
            permission.check_opt(name).and_then(std::env::var_os)
        };
        let case = if settings.try_boolean("core.ignorecase").map_err(other)? == Some(true) {
            Case::Fold
        } else {
            Case::Sensitive
        };
        let mut files = Vec::new();
        for (source, allowed) in [
            (gix::attrs::Source::GitInstallation, permissions.git_binary),
            (gix::attrs::Source::System, permissions.system),
        ] {
            if allowed && let Some(path) = source.storage_location(&mut attribute_env) {
                files.push(path);
            }
        }
        if let Some(path) = settings
            .trusted_path("core.attributesfile")
            .map_err(other)?
        {
            files.push(path);
        } else if permissions.git
            && let Some(path) = gix::attrs::Source::Git.storage_location(&mut attribute_env)
        {
            files.push(path);
        }
        let mut globals = DirectoryRules::default();
        globals.inherited.add_patterns_buffer(
            b"[attr]binary -diff -merge -text\n",
            "[builtin]".into(),
            None,
            &mut globals.collection,
            true,
        );
        let mut hasher = Blake2b512::new();
        hash_bytes(&mut hasher, b"jj-git-attributes-eol-v1");
        hash_bytes(&mut hasher, format!("{mode:?}:{case:?}").as_bytes());
        for path in files {
            let contents = read_optional(&path)?;
            hash_input(&mut hasher, &path, contents.as_deref());
            if let Some(contents) = contents {
                globals.inherited.add_patterns_buffer(
                    &contents,
                    path,
                    None,
                    &mut globals.collection,
                    true,
                );
            }
        }
        // Linked worktrees can spell the shared directory with `../..`.
        // Normalize the directory, since the optional attributes file may be absent.
        let info_path = dunce::canonicalize(repo.common_dir())?.join("info/attributes");
        let info_contents = read_optional(&info_path)?;
        hash_input(&mut hasher, &info_path, info_contents.as_deref());
        let config = eol::Configuration {
            // Git configuration does not override JJ's existing global setting.
            // Explicit text/eol attributes take precedence over that setting.
            auto_crlf: match mode {
                EolConversionMode::None => eol::AutoCrlf::Disabled,
                EolConversionMode::Input => eol::AutoCrlf::Input,
                EolConversionMode::InputOutput => eol::AutoCrlf::Enabled,
            },
            eol: None,
        };
        Ok(Some(Self {
            store,
            tree,
            root,
            source,
            case,
            config,
            globals,
            info_contents,
            directories: Mutex::new(HashMap::new()),
            external_fingerprint: hasher.finalize().to_vec(),
            sparse_matcher: PrefixMatcher::new([RepoPath::root()]),
        }))
    }

    pub(crate) fn set_sparse_patterns(&mut self, patterns: &[RepoPathBuf]) {
        self.sparse_matcher = PrefixMatcher::new(patterns);
        self.directories.get_mut().unwrap().clear();
    }

    pub(crate) fn external_attributes_fingerprint(&self) -> &[u8] {
        &self.external_fingerprint
    }

    async fn stored_attributes(
        &self,
        tree: &MergedTree,
        path: &RepoPath,
    ) -> io::Result<Option<Vec<u8>>> {
        let value = tree.path_value(path).await.map_err(other)?;
        let Some(Some(TreeValue::File { id, .. })) = value.as_resolved() else {
            // Attribute symlinks and unresolved attributes cannot supply a
            // single set of tree rules.
            return Ok(None);
        };
        let mut contents = Vec::new();
        self.store
            .read_file(path, id)
            .await
            .map_err(other)?
            .read_to_end(&mut contents)
            .await?;
        Ok(Some(contents))
    }

    async fn directory_rules(&self, path: &RepoPath) -> io::Result<Arc<DirectoryRules>> {
        let directory = path.parent().unwrap_or(RepoPath::root());
        let mut ancestors = Vec::new();
        let mut next = Some(directory);
        while let Some(current) = next {
            ancestors.push(current.to_owned());
            next = current.parent();
        }
        let attribute_name = RepoPathComponent::new(".gitattributes").unwrap();
        let mut inherited = Arc::new(self.globals.clone());
        for directory in ancestors.into_iter().rev() {
            if let Some(cached) = self.directories.lock().unwrap().get(&directory).cloned() {
                inherited = cached;
                continue;
            }
            let attribute_path = directory.join(attribute_name);
            let stored = self.stored_attributes(&self.tree, &attribute_path).await?;
            let disk_path = attribute_path.to_fs_path(&self.root).map_err(other)?;
            let live = if self.source == EolSource::Snapshot {
                read_worktree_attributes(&self.root, &attribute_path)?
            } else {
                None
            };
            let mut rules = (*inherited).clone();
            let omitted_by_sparse = !self.sparse_matcher.matches(&attribute_path);
            if self.source == EolSource::Snapshot && !omitted_by_sparse {
                rules.changed |= !same_attribute_lines(live.as_deref(), stored.as_deref());
            }
            // An absent tracked attribute file is a deletion. Only sparse
            // omissions use the stored file as their source of rules.
            let contents = if self.source == EolSource::Checkout || omitted_by_sparse {
                stored.as_deref()
            } else {
                live.as_deref()
            };
            rules.contents = contents.map(<[u8]>::to_vec);
            if let Some(contents) = contents {
                rules.inherited.add_patterns_buffer(
                    contents,
                    disk_path,
                    Some(&self.root),
                    &mut rules.collection,
                    directory.is_root(),
                );
            }
            // info/attributes has highest priority, including macro definitions.
            // Keep it separate so child-directory patterns cannot outrank it.
            rules.info = gix::attrs::Search::default();
            if let Some(contents) = &self.info_contents {
                rules.info.add_patterns_buffer(
                    contents,
                    "[info/attributes]".into(),
                    None,
                    &mut rules.collection,
                    true,
                );
            }
            inherited = self
                .directories
                .lock()
                .unwrap()
                .entry(directory)
                .or_insert_with(|| Arc::new(rules))
                .clone();
        }
        Ok(inherited)
    }

    pub(crate) async fn attributes_changed(&self, path: &RepoPath) -> io::Result<bool> {
        Ok(self.directory_rules(path).await?.changed)
    }

    pub(crate) async fn attributes_match_tree(&self, tree: &MergedTree) -> io::Result<bool> {
        // A mutable filesystem can change an attribute file after we read its
        // rules but before the snapshot saves that file. Check the saved tree
        // before the caller records these scan results as reusable.
        let used_rules = self
            .directories
            .lock()
            .unwrap()
            .iter()
            .map(|(directory, rules)| (directory.clone(), rules.contents.clone()))
            .collect::<Vec<_>>();
        let attribute_name = RepoPathComponent::new(".gitattributes").unwrap();
        for (directory, contents) in used_rules {
            let stored = self
                .stored_attributes(tree, &directory.join(attribute_name))
                .await?;
            if !same_attribute_lines(contents.as_deref(), stored.as_deref()) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) async fn rule(&self, path: &RepoPath) -> io::Result<Option<AttributesDigest>> {
        let rules = self.directory_rules(path).await?;
        let mut outcome = Outcome::default();
        outcome.initialize_with_selection(&rules.collection, ["text", "crlf", "eol"]);
        let path = path.as_internal_file_string().as_bytes().as_bstr();
        rules
            .info
            .pattern_matching_relative_path(path, self.case, Some(false), &mut outcome);
        rules
            .inherited
            .pattern_matching_relative_path(path, self.case, Some(false), &mut outcome);
        let states = outcome
            .iter_selected()
            .map(|matched| matched.assignment.state)
            .collect::<Vec<_>>();
        let mut digest = text_digest(states[0]).or_else(|| text_digest(states[1]));
        if digest != Some(AttributesDigest::Binary)
            && let StateRef::Value(value) = states[2]
        {
            digest = match (digest, value.as_bstr().as_bytes()) {
                (Some(AttributesDigest::TextAuto), b"lf") => Some(AttributesDigest::TextAutoInput),
                (Some(AttributesDigest::TextAuto), b"crlf") => Some(AttributesDigest::TextAutoCrlf),
                (_, b"lf") => Some(AttributesDigest::TextInput),
                (_, b"crlf") => Some(AttributesDigest::TextCrlf),
                _ => digest,
            };
        }
        Ok(digest)
    }

    pub(crate) async fn convert_for_snapshot<'a>(
        &self,
        path: &RepoPath,
        mut contents: impl AsyncRead + Send + Unpin + 'a,
        rule: AttributesDigest,
    ) -> io::Result<Box<dyn AsyncRead + Send + Unpin + 'a>> {
        if rule == AttributesDigest::Binary {
            return Ok(Box::new(contents));
        }
        let mut source = Vec::new();
        contents.read_to_end(&mut source).await?;
        let previous_has_crlf = if rule.is_auto_text() && source.contains(&b'\r') {
            let value = self.tree.path_value(path).await.map_err(other)?;
            let mut has_crlf = false;
            for term in value.iter() {
                let Some(TreeValue::File { id, .. }) = term else {
                    continue;
                };
                let mut bytes = Vec::new();
                self.store
                    .read_file(path, id)
                    .await
                    .map_err(other)?
                    .read_to_end(&mut bytes)
                    .await?;
                let stats = eol::Stats::from_bytes(&bytes);
                if !stats.is_binary() && stats.crlf > 0 {
                    has_crlf = true;
                    break;
                }
            }
            has_crlf
        } else {
            false
        };
        if previous_has_crlf {
            // Match Git's text=auto rule for existing CRLF blobs. Apply the
            // same guard to every term of a file conflict.
            return Ok(Box::new(Cursor::new(source)));
        }
        let mut converted = Vec::new();
        let changed = eol::convert_to_git(
            &source,
            rule,
            &mut converted,
            // Existing CRLF blobs were checked above, including conflicts.
            &mut |_| Ok(None),
            eol::convert_to_git::Options {
                config: self.config,
                round_trip_check: None,
            },
        )
        .map_err(other)?;
        Ok(Box::new(Cursor::new(if changed {
            converted
        } else {
            source
        })))
    }

    pub(crate) async fn convert_for_checkout<'a>(
        &self,
        mut contents: impl AsyncRead + Send + Unpin + 'a,
        rule: AttributesDigest,
    ) -> io::Result<Box<dyn AsyncRead + Send + Unpin + 'a>> {
        if rule.to_eol(self.config) != Some(eol::Mode::CrLf) {
            return Ok(Box::new(contents));
        }
        let mut source = Vec::new();
        contents.read_to_end(&mut source).await?;
        let mut converted = Vec::new();
        let changed =
            eol::convert_to_worktree(&source, rule, &mut converted, self.config).map_err(other)?;
        Ok(Box::new(Cursor::new(if changed {
            converted
        } else {
            source
        })))
    }
}

fn text_digest(state: StateRef<'_>) -> Option<AttributesDigest> {
    match state {
        StateRef::Set => Some(AttributesDigest::Text),
        StateRef::Unset => Some(AttributesDigest::Binary),
        StateRef::Value(value) => match value.as_bstr().as_bytes() {
            b"auto" => Some(AttributesDigest::TextAuto),
            b"input" => Some(AttributesDigest::TextInput),
            _ => None,
        },
        StateRef::Unspecified => None,
    }
}

fn same_attribute_lines(left: Option<&[u8]>, right: Option<&[u8]>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.lines().eq(right.lines()),
        (None, None) => true,
        _ => false,
    }
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_worktree_attributes(root: &Path, path: &RepoPath) -> io::Result<Option<Vec<u8>>> {
    // Attribute files and their parent directories must not follow symlinks.
    let mut current = root.to_owned();
    for component in path.components() {
        current.push(component.to_fs_name().map_err(other)?);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(None),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    read_optional(&current)
}

fn hash_bytes(hasher: &mut Blake2b512, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_input(hasher: &mut Blake2b512, path: &Path, contents: Option<&[u8]>) {
    hash_bytes(hasher, path.as_os_str().as_encoded_bytes());
    hasher.update([u8::from(contents.is_some())]);
    if let Some(contents) = contents {
        hash_bytes(hasher, contents);
    }
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StackedConfig;
    use crate::git_backend::GitBackend;
    use crate::settings::UserSettings;
    use crate::signing::Signer;
    use crate::tests::TestResult;
    use crate::tests::new_temp_dir;
    use crate::tree_merge::MergeOptions;

    #[test]
    fn linked_worktree_has_same_external_attributes_fingerprint() -> TestResult {
        let temp_dir = new_temp_dir();
        let main_root = temp_dir.path().join("main");
        let child_root = temp_dir.path().join("child");
        let store_path = temp_dir.path().join("store");
        for path in [&main_root, &child_root, &store_path] {
            fs::create_dir(path)?;
        }
        let settings = UserSettings::from_config(StackedConfig::with_defaults())?;
        let backend =
            GitBackend::init_colocated(&settings, &store_path, &main_root, gix::hash::Kind::Sha1)?;
        let store = Store::new(
            Box::new(backend),
            Signer::new(None, vec![]),
            MergeOptions::from_settings(&settings)?,
        );

        // Keep the relative commondir spelling used by Git linked worktrees.
        let git_dir = main_root.join(".git");
        let child_git_dir = git_dir.join("worktrees/child");
        fs::create_dir_all(&child_git_dir)?;
        fs::write(child_git_dir.join("commondir"), "../..\n")?;
        fs::write(child_git_dir.join("HEAD"), fs::read(git_dir.join("HEAD"))?)?;
        fs::write(
            child_git_dir.join("gitdir"),
            format!("{}\n", child_root.join(".git").display()),
        )?;
        fs::write(
            child_root.join(".git"),
            format!("gitdir: {}\n", child_git_dir.display()),
        )?;

        let fingerprint = |root: &Path| -> TestResult<Vec<u8>> {
            let eol = GitEol::new(
                store.clone(),
                store.empty_merged_tree(),
                root.to_owned(),
                EolConversionMode::None,
                EolSource::Snapshot,
            )?
            .unwrap();
            Ok(eol.external_attributes_fingerprint().to_vec())
        };
        let absent = fingerprint(&main_root)?;
        assert_eq!(fingerprint(&child_root)?, absent);

        let info_dir = git_dir.join("info");
        fs::create_dir_all(&info_dir)?;
        let attributes_path = info_dir.join("attributes");
        fs::write(&attributes_path, "* text eol=lf\n")?;
        let present = fingerprint(&main_root)?;
        assert_eq!(fingerprint(&child_root)?, present);
        assert_ne!(present, absent);

        fs::write(&attributes_path, "* text eol=crlf\n")?;
        let changed = fingerprint(&main_root)?;
        assert_eq!(fingerprint(&child_root)?, changed);
        assert_ne!(changed, present);

        fs::remove_file(attributes_path)?;
        assert_eq!(fingerprint(&main_root)?, absent);
        assert_eq!(fingerprint(&child_root)?, absent);
        Ok(())
    }

    #[test]
    fn attribute_change_ignores_checkout_line_endings() {
        assert!(same_attribute_lines(
            Some(b"* text=auto\r\n*.cmd eol=crlf\r\n"),
            Some(b"* text=auto\n*.cmd eol=crlf\n"),
        ));
        assert!(!same_attribute_lines(Some(b"* text\n"), None));
        assert!(!same_attribute_lines(Some(b"* text\n"), Some(b"* -text\n")));
    }
}
