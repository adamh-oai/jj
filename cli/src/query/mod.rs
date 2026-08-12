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

//! jq-compatible structured queries.

#![allow(clippy::elidable_lifetime_names)]
#![allow(clippy::use_self)]

use std::cmp::Ordering;
use std::fmt;
use std::ops;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::OnceLock;

use base64::Engine as _;
use futures::StreamExt as _;
use indexmap::IndexMap;
use jaq_core::Exn;
use jaq_core::ValR;
use jaq_core::ValX;
use jaq_core::ValXs;
use jaq_core::load::Arena;
use jaq_core::load::File;
use jaq_core::load::Loader;
use jaq_core::path::Opt;
use jaq_json::Num;
use jaq_std::ValT as _;
use jj_lib::backend::MergedTreeValue;
use jj_lib::backend::TreeValue;
use jj_lib::commit::Commit;
use jj_lib::conflicts::ConflictMarkerStyle;
use jj_lib::copies::CopyRecords;
use jj_lib::matchers::Matcher;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::op_store::RemoteRef;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo as _;
use jj_lib::revset::ResolvedRevsetExpression;
use jj_lib::trailer;

use crate::diff_util::DiffStatOptions;
use crate::diff_util::DiffStats;

const COMMIT_FIELDS: [&str; 10] = [
    "schema",
    "commit_id",
    "change_id",
    "parent_ids",
    "description",
    "trailers",
    "author",
    "committer",
    "conflict",
    "root",
];

const SEMANTIC_FILTERS: [&str; 14] = [
    "jj::mine",
    "jj::working_copies",
    "jj::current_working_copy",
    "jj::bookmarks",
    "jj::tags",
    "jj::divergent",
    "jj::hidden",
    "jj::change_offset",
    "jj::immutable",
    "jj::empty",
    "jj::signature_present",
    "jj::verify_signature",
    "jj::diff_files",
    "jj::diff_stats",
];

/// A lazy, read-only commit object used as a jq input value.
pub struct CommitQueryObject {
    commit: Commit,
    repo: Arc<ReadonlyRepo>,
    user_email: Arc<str>,
    workspace_name: WorkspaceNameBuf,
    immutable_expression: Arc<ResolvedRevsetExpression>,
    matcher: Arc<dyn Matcher>,
    diff_stat_options: Arc<DiffStatOptions>,
    conflict_marker_style: ConflictMarkerStyle,
    fields: [OnceLock<QueryValue>; COMMIT_FIELDS.len()],
    semantic: [OnceLock<Result<QueryValue, String>>; SEMANTIC_FILTERS.len()],
}

impl fmt::Debug for CommitQueryObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommitQueryObject")
            .field("commit_id", self.commit.id())
            .finish_non_exhaustive()
    }
}

impl CommitQueryObject {
    /// Constructs a lazy query object from a selected commit.
    pub fn new(
        repo: Arc<ReadonlyRepo>,
        commit: Commit,
        user_email: Arc<str>,
        workspace_name: WorkspaceNameBuf,
        immutable_expression: Arc<ResolvedRevsetExpression>,
        matcher: Arc<dyn Matcher>,
        diff_stat_options: Arc<DiffStatOptions>,
        conflict_marker_style: ConflictMarkerStyle,
    ) -> Self {
        Self {
            commit,
            repo,
            user_email,
            workspace_name,
            immutable_expression,
            matcher,
            diff_stat_options,
            conflict_marker_style,
            fields: std::array::from_fn(|_| OnceLock::new()),
            semantic: std::array::from_fn(|_| OnceLock::new()),
        }
    }

    fn field(&self, name: &str) -> Option<QueryValue> {
        let index = COMMIT_FIELDS
            .iter()
            .position(|candidate| *candidate == name)?;
        Some(
            self.fields[index]
                .get_or_init(|| self.compute_field(index))
                .clone(),
        )
    }

    fn compute_field(&self, index: usize) -> QueryValue {
        match COMMIT_FIELDS[index] {
            "schema" => QueryValue::from("jj.commit/v1"),
            "commit_id" => QueryValue::from(self.commit.id().hex()),
            "change_id" => QueryValue::from(self.commit.change_id().reverse_hex()),
            "parent_ids" => self
                .commit
                .parent_ids()
                .iter()
                .map(|id| QueryValue::from(id.hex()))
                .collect(),
            "description" => QueryValue::from(self.commit.description()),
            "trailers" => trailer::parse_description_trailers(self.commit.description())
                .into_iter()
                .map(|trailer| {
                    object([
                        ("key", QueryValue::from(trailer.key)),
                        ("value", QueryValue::from(trailer.value)),
                    ])
                })
                .collect(),
            "author" => signature(self.commit.author()),
            "committer" => signature(self.commit.committer()),
            "conflict" => QueryValue::Bool(self.commit.has_conflict()),
            "root" => QueryValue::Bool(self.commit.id() == self.repo.store().root_commit_id()),
            _ => unreachable!(),
        }
    }

    fn materialize(&self) -> IndexMap<String, QueryValue> {
        COMMIT_FIELDS
            .iter()
            .map(|name| ((*name).to_owned(), self.field(name).unwrap()))
            .collect()
    }
}

fn signature(signature: &jj_lib::backend::Signature) -> QueryValue {
    object([
        ("name", QueryValue::from(signature.name.clone())),
        ("email", QueryValue::from(signature.email.clone())),
        (
            "timestamp",
            object([
                (
                    "millis_since_epoch",
                    QueryValue::Num(Num::from_integral(signature.timestamp.timestamp.0)),
                ),
                (
                    "utc_offset_minutes",
                    QueryValue::from(signature.timestamp.tz_offset as isize),
                ),
            ]),
        ),
    ])
}

fn object<const N: usize>(entries: [(&str, QueryValue); N]) -> QueryValue {
    QueryValue::Object(Rc::new(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    ))
}

fn checked_number(number: Num) -> ValR<QueryValue> {
    let finite =
        jaq_std::ValT::as_f64(&jaq_json::Val::Num(number.clone())).is_some_and(f64::is_finite);
    finite.then_some(QueryValue::Num(number)).ok_or_else(|| {
        jaq_core::Error::new(object([
            ("schema", QueryValue::from("jj.query-error/v1")),
            ("kind", QueryValue::from("non-finite")),
            ("filter", QueryValue::Null),
            ("commit_id", QueryValue::Null),
        ]))
    })
}

/// A jq value which can retain lazy jj host objects inside constructed values.
#[derive(Clone, Debug, Default)]
pub enum QueryValue {
    #[default]
    Null,
    Bool(bool),
    Num(Num),
    String(Rc<String>),
    Array(Rc<Vec<QueryValue>>),
    Object(Rc<IndexMap<String, QueryValue>>),
    Commit(Rc<CommitQueryObject>),
}

impl QueryValue {
    /// Wraps a commit without materializing its fields.
    pub fn commit(
        repo: Arc<ReadonlyRepo>,
        commit: Commit,
        user_email: Arc<str>,
        workspace_name: WorkspaceNameBuf,
        immutable_expression: Arc<ResolvedRevsetExpression>,
        matcher: Arc<dyn Matcher>,
        diff_stat_options: Arc<DiffStatOptions>,
        conflict_marker_style: ConflictMarkerStyle,
    ) -> Self {
        Self::Commit(Rc::new(CommitQueryObject::new(
            repo,
            commit,
            user_email,
            workspace_name,
            immutable_expression,
            matcher,
            diff_stat_options,
            conflict_marker_style,
        )))
    }

    fn underlying_host_error(&self, error: &Self) -> Option<&str> {
        let Self::Object(fields) = error else {
            return None;
        };
        if fields.get("schema")?.as_string()? != "jj.query-error/v1"
            || fields.get("kind")?.as_string()? != "host"
        {
            return None;
        }
        let filter = fields.get("filter")?.as_string()?;
        let commit_id = fields.get("commit_id")?.as_string()?;
        self.find_host_error(filter, commit_id)
    }

    fn find_host_error(&self, filter: &str, commit_id: &str) -> Option<&str> {
        match self {
            Self::Commit(host) if host.commit.id().hex() == commit_id => {
                let index = SEMANTIC_FILTERS
                    .iter()
                    .position(|candidate| *candidate == filter)?;
                host.semantic[index]
                    .get()
                    .and_then(|result| result.as_ref().err())
                    .map(String::as_str)
            }
            Self::Array(values) => values
                .iter()
                .find_map(|value| value.find_host_error(filter, commit_id)),
            _ => None,
        }
    }

    fn materialized(self) -> Self {
        match self {
            Self::Commit(commit) => Self::Object(Rc::new(commit.materialize())),
            value => value,
        }
    }

    fn materialized_ref(&self) -> QueryValue {
        self.clone().materialized()
    }

    fn as_string(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    fn as_index(&self, len: usize) -> Option<usize> {
        let index = self.as_isize()?;
        let index = if index < 0 {
            len as isize + index
        } else {
            index
        };
        usize::try_from(index).ok().filter(|index| *index < len)
    }

    fn index_opt(self, index: &Self) -> Result<Option<Self>, jaq_core::Error<Self>> {
        use jaq_core::ValT as _;
        match (self, index) {
            (Self::Null, _) => Ok(None),
            (Self::Array(values), Self::Object(range)) => {
                let start = range.get("start");
                let end = range.get("end");
                Self::Array(values).range(start..end).map(Some)
            }
            (Self::Array(values), index) => Ok(index
                .as_index(values.len())
                .map(|index| values[index].clone())),
            (Self::Object(values), Self::String(key)) => Ok(values.get(key.as_str()).cloned()),
            (Self::Commit(commit), Self::String(key)) => Ok(commit.field(key)),
            (value @ Self::String(_), Self::Object(range)) => {
                let start = range.get("start");
                let end = range.get("end");
                value.range(start..end).map(Some)
            }
            (value, index) => Err(jaq_core::Error::index(value, index.clone())),
        }
    }

    fn range_bounds(
        range: ops::Range<Option<&Self>>,
        len: usize,
    ) -> Result<(usize, usize), jaq_core::Error<Self>> {
        let bound = |value: Option<&Self>, default: usize| {
            let Some(value) = value.filter(|value| !matches!(value, Self::Null)) else {
                return Ok(default);
            };
            let index = value
                .as_isize()
                .ok_or_else(|| jaq_core::Error::typ(value.clone(), "integer"))?;
            Ok::<_, jaq_core::Error<Self>>(if index < 0 {
                len.saturating_sub(index.unsigned_abs())
            } else {
                usize::try_from(index).unwrap_or(usize::MAX).min(len)
            })
        };
        let start = bound(range.start, 0)?;
        let end = bound(range.end, len)?;
        Ok((start, end.saturating_sub(start)))
    }

    fn into_array(self) -> Result<Vec<Self>, jaq_core::Error<Self>> {
        match self {
            Self::Array(values) => Ok(Rc::unwrap_or_clone(values)),
            value => Err(jaq_core::Error::typ(value, "array")),
        }
    }

    fn write_json(&self, output: &mut Vec<u8>) -> Result<(), String> {
        match self {
            Self::Null => output.extend_from_slice(b"null"),
            Self::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
            Self::Num(value) => {
                let text = value.to_string();
                text.parse::<serde_json::Number>()
                    .map_err(|_| "query produced a non-finite number".to_owned())?;
                output.extend_from_slice(text.as_bytes());
            }
            Self::String(value) => {
                serde_json::to_writer(output, value.as_str()).map_err(|err| err.to_string())?;
            }
            Self::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    value.write_json(output)?;
                }
                output.push(b']');
            }
            Self::Object(values) => {
                output.push(b'{');
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    serde_json::to_writer(&mut *output, key).map_err(|err| err.to_string())?;
                    output.push(b':');
                    value.write_json(output)?;
                }
                output.push(b'}');
            }
            Self::Commit(commit) => {
                Self::Object(Rc::new(commit.materialize())).write_json(output)?;
            }
        }
        Ok(())
    }

    /// Serializes one complete compact JSON record into a temporary buffer.
    pub fn to_json_record(&self) -> Result<Vec<u8>, String> {
        let mut output = Vec::new();
        self.write_json(&mut output)?;
        Ok(output)
    }
}

impl jaq_core::ValT for QueryValue {
    fn from_num(number: &str) -> ValR<Self> {
        let value = <jaq_json::Val as jaq_core::ValT>::from_num(number)
            .map_err(|error| jaq_core::Error::str(error.to_string()))?;
        let jaq_json::Val::Num(number) = value else {
            unreachable!()
        };
        checked_number(number)
    }

    fn from_map<I: IntoIterator<Item = (Self, Self)>>(entries: I) -> ValR<Self> {
        let mut object = IndexMap::new();
        for (key, value) in entries {
            let Self::String(key) = key else {
                return Err(jaq_core::Error::typ(key, "string object key"));
            };
            object.insert((*key).clone(), value);
        }
        Ok(Self::Object(Rc::new(object)))
    }

    fn key_values(self) -> Box<dyn Iterator<Item = Result<(Self, Self), jaq_core::Error<Self>>>> {
        match self {
            Self::Array(values) => Box::new(
                Rc::unwrap_or_clone(values)
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| Ok((Self::from(index), value))),
            ),
            Self::Object(values) => Box::new(
                Rc::unwrap_or_clone(values)
                    .into_iter()
                    .map(|(key, value)| Ok((Self::from(key), value))),
            ),
            Self::Commit(commit) => Box::new(
                COMMIT_FIELDS
                    .into_iter()
                    .map(move |name| Ok((Self::from(name), commit.field(name).unwrap()))),
            ),
            value => Box::new(std::iter::once(Err(jaq_core::Error::typ(
                value,
                "iterable (array or object)",
            )))),
        }
    }

    fn values(self) -> Box<dyn Iterator<Item = ValR<Self>>> {
        match self {
            Self::Array(values) => Box::new(Rc::unwrap_or_clone(values).into_iter().map(Ok)),
            Self::Object(values) => Box::new(
                Rc::unwrap_or_clone(values)
                    .into_iter()
                    .map(|(_, value)| Ok(value)),
            ),
            Self::Commit(commit) => Box::new(
                COMMIT_FIELDS
                    .into_iter()
                    .map(move |name| Ok(commit.field(name).unwrap())),
            ),
            value => Box::new(std::iter::once(Err(jaq_core::Error::typ(
                value,
                "iterable (array or object)",
            )))),
        }
    }

    fn index(self, index: &Self) -> ValR<Self> {
        self.index_opt(index)
            .map(|value| value.unwrap_or(Self::Null))
    }

    fn range(self, range: ops::Range<Option<&Self>>) -> ValR<Self> {
        match self {
            Self::Array(values) => {
                let (skip, take) = Self::range_bounds(range, values.len())?;
                Ok(values.iter().skip(skip).take(take).cloned().collect())
            }
            Self::String(value) => {
                let chars: Vec<char> = value.chars().collect();
                let (skip, take) = Self::range_bounds(range, chars.len())?;
                Ok(chars[skip..skip + take].iter().collect::<String>().into())
            }
            value => Err(jaq_core::Error::typ(value, "rangeable (array or string)")),
        }
    }

    fn map_values<'a, I: Iterator<Item = ValX<'a, Self>>>(
        self,
        opt: Opt,
        f: impl Fn(Self) -> I,
    ) -> ValX<'a, Self> {
        match self.materialized() {
            Self::Array(values) => Ok(values
                .iter()
                .cloned()
                .flat_map(f)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .collect()),
            Self::Object(values) => {
                let mut output = IndexMap::new();
                for (key, value) in values.iter() {
                    if let Some(value) = f(value.clone()).next().transpose()? {
                        output.insert(key.clone(), value);
                    }
                }
                Ok(Self::Object(Rc::new(output)))
            }
            value => opt.fail(value, |value| {
                Exn::from(jaq_core::Error::typ(value, "iterable (array or object)"))
            }),
        }
    }

    fn map_index<'a, I: Iterator<Item = ValX<'a, Self>>>(
        self,
        index: &Self,
        opt: Opt,
        f: impl Fn(Self) -> I,
    ) -> ValX<'a, Self> {
        match self.materialized() {
            Self::Object(values) => {
                let Self::String(key) = index else {
                    return opt.fail(Self::Object(values), |value| {
                        Exn::from(jaq_core::Error::index(value, index.clone()))
                    });
                };
                let mut values = Rc::unwrap_or_clone(values);
                let old = values.shift_remove(key.as_str()).unwrap_or(Self::Null);
                if let Some(value) = f(old).next().transpose()? {
                    values.insert((**key).clone(), value);
                }
                Ok(Self::Object(Rc::new(values)))
            }
            Self::Array(values) => {
                let Some(position) = index.as_index(values.len()) else {
                    return opt.fail(Self::Array(values), |value| {
                        Exn::from(jaq_core::Error::index(value, index.clone()))
                    });
                };
                let mut values = Rc::unwrap_or_clone(values);
                let old = std::mem::take(&mut values[position]);
                if let Some(value) = f(old).next().transpose()? {
                    values[position] = value;
                } else {
                    values.remove(position);
                }
                Ok(Self::Array(Rc::new(values)))
            }
            value => opt.fail(value, |value| {
                Exn::from(jaq_core::Error::index(value, index.clone()))
            }),
        }
    }

    fn map_range<'a, I: Iterator<Item = ValX<'a, Self>>>(
        self,
        range: ops::Range<Option<&Self>>,
        opt: Opt,
        f: impl Fn(Self) -> I,
    ) -> ValX<'a, Self> {
        match self {
            Self::Array(values) => {
                let (skip, take) = Self::range_bounds(range, values.len()).map_err(Exn::from)?;
                let mut values = Rc::unwrap_or_clone(values);
                let selected = values[skip..skip + take].iter().cloned().collect();
                let replacement = f(selected)
                    .next()
                    .transpose()?
                    .unwrap_or_default()
                    .into_array()
                    .map_err(Exn::from)?;
                values.splice(skip..skip + take, replacement);
                Ok(Self::Array(Rc::new(values)))
            }
            value => opt.fail(value, |value| {
                Exn::from(jaq_core::Error::typ(value, "array"))
            }),
        }
    }

    fn as_bool(&self) -> bool {
        !matches!(self, Self::Null | Self::Bool(false))
    }

    fn into_string(self) -> Self {
        match self {
            value @ Self::String(_) => value,
            value => String::from_utf8(value.to_json_record().expect("displayable query value"))
                .unwrap()
                .into(),
        }
    }
}

impl jaq_std::ValT for QueryValue {
    fn into_seq<S: FromIterator<Self>>(self) -> Result<S, Self> {
        match self {
            Self::Array(values) => Ok(Rc::unwrap_or_clone(values).into_iter().collect()),
            value => Err(value),
        }
    }

    fn is_int(&self) -> bool {
        matches!(self, Self::Num(number) if jaq_std::ValT::is_int(&jaq_json::Val::Num(number.clone())))
    }

    fn as_isize(&self) -> Option<isize> {
        match self {
            Self::Num(number) => number.as_isize(),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(number) => jaq_std::ValT::as_f64(&jaq_json::Val::Num(number.clone())),
            _ => None,
        }
    }

    fn is_utf8_str(&self) -> bool {
        matches!(self, Self::String(_))
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        self.as_string().map(str::as_bytes)
    }

    fn as_sub_str(&self, sub: &[u8]) -> Self {
        debug_assert!(self.as_bytes().is_some());
        String::from_utf8(sub.to_vec()).unwrap().into()
    }

    fn from_utf8_bytes(bytes: impl AsRef<[u8]> + Send + 'static) -> Self {
        String::from_utf8(bytes.as_ref().to_vec()).unwrap().into()
    }
}

impl From<bool> for QueryValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<isize> for QueryValue {
    fn from(value: isize) -> Self {
        Self::Num(Num::Int(value))
    }
}

impl From<usize> for QueryValue {
    fn from(value: usize) -> Self {
        Self::Num(Num::from_integral(value))
    }
}

impl From<f64> for QueryValue {
    fn from(value: f64) -> Self {
        Self::Num(Num::Float(value))
    }
}

impl From<String> for QueryValue {
    fn from(value: String) -> Self {
        Self::String(Rc::new(value))
    }
}

impl From<&str> for QueryValue {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}

impl From<ops::Range<Option<QueryValue>>> for QueryValue {
    fn from(range: ops::Range<Option<QueryValue>>) -> Self {
        Self::Object(Rc::new(
            [("start", range.start), ("end", range.end)]
                .into_iter()
                .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value)))
                .collect(),
        ))
    }
}

impl FromIterator<QueryValue> for QueryValue {
    fn from_iter<T: IntoIterator<Item = QueryValue>>(iter: T) -> Self {
        Self::Array(Rc::new(iter.into_iter().collect()))
    }
}

impl ops::Add for QueryValue {
    type Output = ValR<Self>;
    fn add(self, rhs: Self) -> Self::Output {
        match (self.materialized(), rhs.materialized()) {
            (Self::Null, value) | (value, Self::Null) => Ok(value),
            (Self::Num(left), Self::Num(right)) => checked_number(left + right),
            (Self::String(left), Self::String(right)) => Ok(Self::from(format!("{left}{right}"))),
            (Self::Array(left), Self::Array(right)) => Ok(left
                .iter()
                .chain(right.iter())
                .cloned()
                .collect::<QueryValue>()),
            (Self::Object(left), Self::Object(right)) => {
                let mut output = Rc::unwrap_or_clone(left);
                output.extend(right.iter().map(|(k, v)| (k.clone(), v.clone())));
                Ok(Self::Object(Rc::new(output)))
            }
            (left, right) => Err(jaq_core::Error::math(left, jaq_core::ops::Math::Add, right)),
        }
    }
}

impl ops::Sub for QueryValue {
    type Output = ValR<Self>;
    fn sub(self, rhs: Self) -> Self::Output {
        match (self.materialized(), rhs.materialized()) {
            (Self::Num(left), Self::Num(right)) => checked_number(left - right),
            (Self::Array(left), Self::Array(right)) => Ok(left
                .iter()
                .filter(|value| !right.contains(value))
                .cloned()
                .collect()),
            (left, right) => Err(jaq_core::Error::math(left, jaq_core::ops::Math::Sub, right)),
        }
    }
}

impl ops::Mul for QueryValue {
    type Output = ValR<Self>;
    fn mul(self, rhs: Self) -> Self::Output {
        match (self.materialized(), rhs.materialized()) {
            (Self::Num(left), Self::Num(right)) => checked_number(left * right),
            (Self::String(value), Self::Num(times)) | (Self::Num(times), Self::String(value)) => {
                let Some(times) = times.as_isize() else {
                    return Err(jaq_core::Error::str("string multiplier must be an integer"));
                };
                if times <= 0 {
                    Ok(Self::Null)
                } else {
                    Ok(Self::from(value.repeat(times as usize)))
                }
            }
            (Self::Object(left), Self::Object(right)) => {
                let mut output = Rc::unwrap_or_clone(left);
                output.extend(right.iter().map(|(k, v)| (k.clone(), v.clone())));
                Ok(Self::Object(Rc::new(output)))
            }
            (left, right) => Err(jaq_core::Error::math(left, jaq_core::ops::Math::Mul, right)),
        }
    }
}

impl ops::Div for QueryValue {
    type Output = ValR<Self>;
    fn div(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (Self::Num(left), Self::Num(right)) => checked_number(left / right),
            (Self::String(left), Self::String(right)) => {
                Ok(left.split(right.as_str()).map(QueryValue::from).collect())
            }
            (left, right) => Err(jaq_core::Error::math(left, jaq_core::ops::Math::Div, right)),
        }
    }
}

impl ops::Rem for QueryValue {
    type Output = ValR<Self>;
    fn rem(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (Self::Num(left), Self::Num(right)) => checked_number(left % right),
            (left, right) => Err(jaq_core::Error::math(left, jaq_core::ops::Math::Rem, right)),
        }
    }
}

impl ops::Neg for QueryValue {
    type Output = ValR<Self>;
    fn neg(self) -> Self::Output {
        match self {
            Self::Num(value) => checked_number(-value),
            value => Err(jaq_core::Error::typ(value, "number")),
        }
    }
}

impl PartialEq for QueryValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Num(left), Self::Num(right)) => left == right,
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => left == right,
            (Self::Object(left), Self::Object(right)) => left == right,
            (Self::Commit(_), _) | (_, Self::Commit(_)) => {
                self.materialized_ref() == other.materialized_ref()
            }
            _ => false,
        }
    }
}

impl Eq for QueryValue {}

impl PartialOrd for QueryValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueryValue {
    fn cmp(&self, other: &Self) -> Ordering {
        let left = self.materialized_ref();
        let right = other.materialized_ref();
        let rank = |value: &Self| match value {
            Self::Null => 0,
            Self::Bool(_) => 1,
            Self::Num(_) => 2,
            Self::String(_) => 3,
            Self::Array(_) => 4,
            Self::Object(_) | Self::Commit(_) => 5,
        };
        rank(&left)
            .cmp(&rank(&right))
            .then_with(|| match (&left, &right) {
                (Self::Null, Self::Null) => Ordering::Equal,
                (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
                (Self::Num(a), Self::Num(b)) => a.cmp(b),
                (Self::String(a), Self::String(b)) => a.cmp(b),
                (Self::Array(a), Self::Array(b)) => a.cmp(b),
                (Self::Object(a), Self::Object(b)) => {
                    let mut a: Vec<_> = a.iter().collect();
                    let mut b: Vec<_> = b.iter().collect();
                    a.sort_by_key(|(key, _)| *key);
                    b.sort_by_key(|(key, _)| *key);
                    a.cmp(&b)
                }
                _ => Ordering::Equal,
            })
    }
}

impl fmt::Display for QueryValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.to_json_record().map_err(|_| fmt::Error)?;
        formatter.write_str(std::str::from_utf8(&value).map_err(|_| fmt::Error)?)
    }
}

/// Compiled v1 query program.
pub struct QueryProgram {
    filter: jaq_core::Filter<QueryData>,
}

type QueryData = jaq_core::data::JustLut<QueryValue>;

impl QueryProgram {
    /// Compiles a query before any selected commit is evaluated.
    pub fn compile(source: &str) -> Result<Self, String> {
        validate_source(source)?;
        let code = format!("import \"jj\" as jj; {source}");
        let arena = Arena::default();
        let v1_defs = jaq_core::load::parse(include_str!("v1.jq"), |parser| parser.defs())
            .expect("v1 query definitions must parse");
        let loader = Loader::new(jaq_core::defs().chain(v1_defs)).with_read(
            |import: jaq_core::load::Import<'_, &str, String>| {
                if *import.path == "jj" {
                    Ok(File {
                        code: include_str!("jj.jq").to_owned(),
                        path: "<jj>".to_owned(),
                    })
                } else {
                    Err("user modules are not supported in query v1".to_owned())
                }
            },
        );
        let modules = loader
            .load(
                &arena,
                File {
                    code: &code,
                    path: "<query>".to_owned(),
                },
            )
            .map_err(|errors| format!("query parse error: {errors:?}"))?;
        let allowed_native = [
            "explode",
            "ascii_downcase",
            "ascii_upcase",
            "reverse",
            "sort",
            "sort_by",
            "group_by",
            "min_by_or_empty",
            "max_by_or_empty",
            "startswith",
            "endswith",
            "ltrimstr",
            "rtrimstr",
        ];
        let std_funs = jaq_std::base_funs().filter(|(name, _, _)| allowed_native.contains(name));
        let core_funs =
            jaq_core::funs::<QueryData>().filter(|(name, _, _)| *name != "keys_unsorted");
        let filter = jaq_core::Compiler::default()
            .with_funs(core_funs.chain(std_funs).chain(query_funs()))
            .compile(modules)
            .map_err(|errors| format!("query compile error: {errors:?}"))?;
        Ok(Self { filter })
    }

    /// Evaluates the program on one input, preserving jq result cardinality.
    pub fn run(&self, input: QueryValue) -> impl Iterator<Item = Result<QueryValue, String>> + '_ {
        let input_for_errors = input.clone();
        let context = jaq_core::Ctx::<QueryData>::new(&self.filter.lut, jaq_core::Vars::new([]));
        self.filter
            .id
            .run((context, input))
            .map(jaq_core::unwrap_valr)
            .map(move |result| {
                result.map_err(|error| {
                    let error_value = error.clone().into_val();
                    let mut diagnostic = error.to_string();
                    if let Some(source) = input_for_errors.underlying_host_error(&error_value) {
                        diagnostic.push_str(": ");
                        diagnostic.push_str(source);
                    }
                    diagnostic
                })
            })
    }
}

fn query_funs() -> impl Iterator<Item = jaq_core::native::Fun<QueryData>> {
    use jaq_core::native::run;
    use jaq_core::native::v;

    vec![
        run::<QueryData>(("length", v(0), native_length as jaq_core::RunPtr<QueryData>)),
        run::<QueryData>((
            "keys_unsorted",
            v(0),
            native_keys_unsorted as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>(("has", v(1), native_has as jaq_core::RunPtr<QueryData>)),
        run::<QueryData>((
            "contains",
            v(1),
            native_contains as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "indices",
            v(1),
            native_indices as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>(("tojson", v(0), native_tojson as jaq_core::RunPtr<QueryData>)),
        run::<QueryData>((
            "fromjson",
            v(0),
            native_fromjson as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_encode_base64",
            v(0),
            native_encode_base64 as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_encode_uri",
            v(0),
            native_encode_uri as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_format_csv",
            v(0),
            native_format_csv as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_format_tsv",
            v(0),
            native_format_tsv as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_mine",
            v(0),
            native_jj_mine as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_working_copies",
            v(0),
            native_jj_working_copies as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_current_working_copy",
            v(0),
            native_jj_current_working_copy as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_bookmarks",
            v(0),
            native_jj_bookmarks as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_tags",
            v(0),
            native_jj_tags as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_divergent",
            v(0),
            native_jj_divergent as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_hidden",
            v(0),
            native_jj_hidden as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_change_offset",
            v(0),
            native_jj_change_offset as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_immutable",
            v(0),
            native_jj_immutable as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_empty",
            v(0),
            native_jj_empty as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_signature_present",
            v(0),
            native_jj_signature_present as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_verify_signature",
            v(0),
            native_jj_verify_signature as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_diff_files",
            v(0),
            native_jj_diff_files as jaq_core::RunPtr<QueryData>,
        )),
        run::<QueryData>((
            "__jj_diff_stats",
            v(0),
            native_jj_diff_stats as jaq_core::RunPtr<QueryData>,
        )),
    ]
    .into_iter()
}

fn native_length<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    let result = match cv.1 {
        QueryValue::Null => Ok(0isize.into()),
        QueryValue::Num(number) => {
            let text = number.to_string();
            <QueryValue as jaq_core::ValT>::from_num(text.strip_prefix('-').unwrap_or(&text))
        }
        QueryValue::String(value) => Ok(value.chars().count().into()),
        QueryValue::Array(value) => Ok(value.len().into()),
        QueryValue::Object(value) => Ok(value.len().into()),
        QueryValue::Commit(_) => Ok(COMMIT_FIELDS.len().into()),
        value @ QueryValue::Bool(_) => Err(jaq_core::Error::str(format!("{value} has no length"))),
    };
    jaq_core::native::bome(result)
}

fn native_keys_unsorted<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    let result = match cv.1 {
        QueryValue::Array(values) => Ok((0..values.len()).map(QueryValue::from).collect()),
        QueryValue::Object(values) => Ok(values.keys().map(|key| key.as_str().into()).collect()),
        QueryValue::Commit(_) => Ok(COMMIT_FIELDS.into_iter().map(QueryValue::from).collect()),
        value => Err(jaq_core::Error::typ(value, "iterable (array or object)")),
    };
    jaq_core::native::bome(result)
}

fn native_has<'a>(mut cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    let key = cv.0.pop_var();
    let present = match (&cv.1, &key) {
        (QueryValue::Commit(_), QueryValue::String(key)) => COMMIT_FIELDS.contains(&key.as_str()),
        (QueryValue::Object(values), QueryValue::String(key)) => values.contains_key(key.as_str()),
        (QueryValue::Array(values), key) => key.as_index(values.len()).is_some(),
        _ => false,
    };
    jaq_core::native::bome(Ok(present.into()))
}

fn native_contains<'a>(mut cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    let other = cv.0.pop_var();
    fn contains(value: &QueryValue, other: &QueryValue) -> bool {
        match (value, other) {
            (QueryValue::String(value), QueryValue::String(other)) => {
                value.contains(other.as_str())
            }
            (QueryValue::Array(values), QueryValue::Array(others)) => others
                .iter()
                .all(|other| values.iter().any(|value| contains(value, other))),
            (QueryValue::Object(values), QueryValue::Object(others)) => others
                .iter()
                .all(|(key, other)| values.get(key).is_some_and(|value| contains(value, other))),
            (QueryValue::Commit(_), _) | (_, QueryValue::Commit(_)) => {
                contains(&value.materialized_ref(), &other.materialized_ref())
            }
            _ => value == other,
        }
    }
    jaq_core::native::bome(Ok(contains(&cv.1, &other).into()))
}

fn native_indices<'a>(mut cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    let needle = cv.0.pop_var();
    let result = match (&cv.1, &needle) {
        (QueryValue::String(value), QueryValue::String(needle)) if needle.is_empty() => {
            Ok(std::iter::empty().collect())
        }
        (QueryValue::String(value), QueryValue::String(needle)) => Ok(value
            .match_indices(needle.as_str())
            .map(|(byte, _)| QueryValue::from(value[..byte].chars().count()))
            .collect()),
        (QueryValue::Array(values), QueryValue::Array(needle)) if needle.is_empty() => {
            Ok(std::iter::empty().collect())
        }
        (QueryValue::Array(values), QueryValue::Array(needle)) => Ok(values
            .windows(needle.len())
            .enumerate()
            .filter(|(_, values)| *values == needle.as_slice())
            .map(|(index, _)| index.into())
            .collect()),
        (QueryValue::Array(values), needle) => Ok(values
            .iter()
            .enumerate()
            .filter(|(_, value)| *value == needle)
            .map(|(index, _)| index.into())
            .collect()),
        _ => Err(jaq_core::Error::index(cv.1, needle)),
    };
    jaq_core::native::bome(result)
}

fn native_tojson<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    jaq_core::native::bome(
        cv.1.to_json_record()
            .and_then(|bytes| String::from_utf8(bytes).map_err(|err| err.to_string()))
            .map(QueryValue::from)
            .map_err(jaq_core::Error::str),
    )
}

fn native_fromjson<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    let result =
        cv.1.as_string()
            .ok_or_else(|| jaq_core::Error::typ(cv.1.clone(), "string"))
            .and_then(|source| {
                serde_json::from_str::<serde_json::Value>(source)
                    .map_err(jaq_core::Error::str)
                    .and_then(query_value_from_json)
            });
    jaq_core::native::bome(result)
}

fn native_encode_base64<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    let result =
        cv.1.as_string()
            .map(|value| {
                base64::engine::general_purpose::STANDARD
                    .encode(value)
                    .into()
            })
            .ok_or_else(|| jaq_core::Error::typ(cv.1, "string"));
    jaq_core::native::bome(result)
}

fn native_encode_uri<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    let result =
        cv.1.as_string()
            .map(|value| urlencoding::encode(value).into_owned().into())
            .ok_or_else(|| jaq_core::Error::typ(cv.1, "string"));
    jaq_core::native::bome(result)
}

fn format_delimited(input: QueryValue, separator: char, csv: bool) -> ValR<QueryValue> {
    let QueryValue::Array(values) = input else {
        return Err(jaq_core::Error::typ(input, "array"));
    };
    let mut output = String::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(separator);
        }
        match value {
            QueryValue::Null => {}
            QueryValue::Bool(_) | QueryValue::Num(_) => output.push_str(&value.to_string()),
            QueryValue::String(value) if csv => {
                output.push('"');
                output.push_str(&value.replace('"', "\"\""));
                output.push('"');
            }
            QueryValue::String(value) => {
                for character in value.chars() {
                    match character {
                        '\\' => output.push_str("\\\\"),
                        '\t' => output.push_str("\\t"),
                        '\r' => output.push_str("\\r"),
                        '\n' => output.push_str("\\n"),
                        character => output.push(character),
                    }
                }
            }
            value => return Err(jaq_core::Error::typ(value.clone(), "scalar")),
        }
    }
    Ok(output.into())
}

fn native_format_csv<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    jaq_core::native::bome(format_delimited(cv.1, ',', true))
}

fn native_format_tsv<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    jaq_core::native::bome(format_delimited(cv.1, '\t', false))
}

fn query_value_from_json(value: serde_json::Value) -> ValR<QueryValue> {
    Ok(match value {
        serde_json::Value::Null => QueryValue::Null,
        serde_json::Value::Bool(value) => value.into(),
        serde_json::Value::Number(value) => {
            <QueryValue as jaq_core::ValT>::from_num(&value.to_string())?
        }
        serde_json::Value::String(value) => value.into(),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(query_value_from_json)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .collect(),
        serde_json::Value::Object(values) => QueryValue::Object(Rc::new(
            values
                .into_iter()
                .map(|(key, value)| Ok((key, query_value_from_json(value)?)))
                .collect::<Result<_, jaq_core::Error<QueryValue>>>()?,
        )),
    })
}

fn with_commit<'a>(
    input: QueryValue,
    filter: &'static str,
    f: impl FnOnce(&CommitQueryObject) -> ValR<QueryValue>,
) -> ValXs<'a, QueryValue> {
    let result = match input {
        QueryValue::Commit(commit) => f(&commit),
        _ => Err(jaq_core::Error::new(object([
            ("schema", QueryValue::from("jj.query-error/v1")),
            ("kind", QueryValue::from("type")),
            ("filter", QueryValue::from(filter)),
            ("commit_id", QueryValue::Null),
        ]))),
    };
    jaq_core::native::bome(result)
}

fn host_error(host: &CommitQueryObject, filter: &'static str) -> jaq_core::Error<QueryValue> {
    jaq_core::Error::new(object([
        ("schema", QueryValue::from("jj.query-error/v1")),
        ("kind", QueryValue::from("host")),
        ("filter", QueryValue::from(filter)),
        ("commit_id", QueryValue::from(host.commit.id().hex())),
    ]))
}

fn cached_semantic(
    host: &CommitQueryObject,
    filter: &'static str,
    compute: impl FnOnce() -> Result<QueryValue, String>,
) -> ValR<QueryValue> {
    let index = SEMANTIC_FILTERS
        .iter()
        .position(|candidate| *candidate == filter)
        .unwrap();
    match host.semantic[index].get_or_init(compute).clone() {
        Ok(value) => Ok(value),
        Err(_error) => Err(host_error(host, filter)),
    }
}

fn native_jj_mine<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::mine", |host| {
        cached_semantic(host, "jj::mine", || {
            Ok((host.commit.author().email == host.user_email.as_ref()).into())
        })
    })
}

fn native_jj_working_copies<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::working_copies", |host| {
        cached_semantic(host, "jj::working_copies", || {
            let mut workspaces: Vec<_> = host
                .repo
                .view()
                .wc_commit_ids()
                .iter()
                .filter(|(_, id)| *id == host.commit.id())
                .map(|(name, _)| name.as_str().to_owned())
                .collect();
            workspaces.sort();
            Ok(workspaces
                .into_iter()
                .map(|name| {
                    object([
                        ("name", QueryValue::from(name.clone())),
                        (
                            "current",
                            QueryValue::from(name == host.workspace_name.as_str()),
                        ),
                    ])
                })
                .collect())
        })
    })
}

fn native_jj_current_working_copy<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::current_working_copy", |host| {
        cached_semantic(host, "jj::current_working_copy", || {
            Ok(host
                .repo
                .view()
                .get_wc_commit_id(&host.workspace_name)
                .is_some_and(|id| id == host.commit.id())
                .into())
        })
    })
}

fn target_contains(target: &RefTarget, commit: &Commit) -> bool {
    target.as_normal() == Some(commit.id())
        || target.removed_ids().any(|id| id == commit.id())
        || target.added_ids().any(|id| id == commit.id())
}

fn ref_object(
    name: &str,
    remote: Option<&str>,
    target: &RefTarget,
    tracked: bool,
    tracking_present: bool,
    synced: bool,
) -> QueryValue {
    let conflict = target.has_conflict();
    let normal_target = (!conflict)
        .then(|| target.as_normal().map(|id| QueryValue::from(id.hex())))
        .flatten()
        .unwrap_or(QueryValue::Null);
    let mut removed: Vec<_> = if conflict {
        target.removed_ids().map(|id| id.hex()).collect()
    } else {
        vec![]
    };
    let mut added: Vec<_> = if conflict {
        target.added_ids().map(|id| id.hex()).collect()
    } else {
        vec![]
    };
    removed.sort();
    added.sort();
    object([
        ("name", name.into()),
        (
            "remote",
            remote.map(QueryValue::from).unwrap_or(QueryValue::Null),
        ),
        ("present", target.is_present().into()),
        ("conflict", conflict.into()),
        ("normal_target_id", normal_target),
        (
            "removed_target_ids",
            removed.into_iter().map(QueryValue::from).collect(),
        ),
        (
            "added_target_ids",
            added.into_iter().map(QueryValue::from).collect(),
        ),
        ("tracked", tracked.into()),
        ("tracking_present", tracking_present.into()),
        ("synced", synced.into()),
    ])
}

fn refs_for_commit(host: &CommitQueryObject, tags: bool) -> QueryValue {
    type RefGroup = (String, RefTarget, Vec<(String, RemoteRef)>);
    let groups: Vec<RefGroup> = if tags {
        host.repo
            .view()
            .tags()
            .map(|(name, refs)| {
                (
                    name.as_str().to_owned(),
                    refs.local_target.clone(),
                    refs.remote_refs
                        .into_iter()
                        .map(|(name, remote)| (name.as_str().to_owned(), remote.clone()))
                        .collect(),
                )
            })
            .collect()
    } else {
        host.repo
            .view()
            .bookmarks()
            .map(|(name, refs)| {
                (
                    name.as_str().to_owned(),
                    refs.local_target.clone(),
                    refs.remote_refs
                        .into_iter()
                        .map(|(name, remote)| (name.as_str().to_owned(), remote.clone()))
                        .collect(),
                )
            })
            .collect()
    };
    let mut output = Vec::new();
    for (name, local, remotes) in groups {
        if target_contains(&local, &host.commit) {
            let synced = remotes
                .iter()
                .all(|(_, remote)| !remote.is_tracked() || remote.target == local);
            output.push(ref_object(&name, None, &local, false, false, synced));
        }
        for (remote_name, remote) in remotes {
            if target_contains(&remote.target, &host.commit) {
                let tracked = remote.is_tracked();
                output.push(ref_object(
                    &name,
                    Some(&remote_name),
                    &remote.target,
                    tracked,
                    tracked && local.is_present(),
                    tracked && remote.target == local,
                ));
            }
        }
    }
    output.sort_by(|left, right| {
        let get = |value: &QueryValue, key| {
            let QueryValue::Object(value) = value else {
                unreachable!()
            };
            value.get(key).unwrap().clone()
        };
        get(left, "name")
            .cmp(&get(right, "name"))
            .then_with(|| get(left, "remote").cmp(&get(right, "remote")))
    });
    output.into_iter().collect()
}

fn native_jj_bookmarks<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::bookmarks", |host| {
        cached_semantic(host, "jj::bookmarks", || Ok(refs_for_commit(host, false)))
    })
}

fn native_jj_tags<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::tags", |host| {
        cached_semantic(host, "jj::tags", || Ok(refs_for_commit(host, true)))
    })
}

fn native_jj_divergent<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::divergent", |host| {
        cached_semantic(host, "jj::divergent", || {
            let targets = pollster::block_on(host.repo.resolve_change_id(host.commit.change_id()))
                .map_err(|error| error.to_string())?;
            Ok(targets.is_some_and(|targets| targets.is_divergent()).into())
        })
    })
}

fn native_jj_hidden<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::hidden", |host| {
        cached_semantic(host, "jj::hidden", || {
            pollster::block_on(host.commit.is_hidden(host.repo.as_ref()))
                .map(QueryValue::from)
                .map_err(|error| error.to_string())
        })
    })
}

fn native_jj_change_offset<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::change_offset", |host| {
        cached_semantic(host, "jj::change_offset", || {
            let targets = pollster::block_on(host.repo.resolve_change_id(host.commit.change_id()))
                .map_err(|error| error.to_string())?;
            Ok(targets
                .and_then(|targets| targets.find_offset(host.commit.id()))
                .map(QueryValue::from)
                .unwrap_or(QueryValue::Null))
        })
    })
}

fn native_jj_immutable<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::immutable", |host| {
        cached_semantic(host, "jj::immutable", || {
            let revset = host
                .immutable_expression
                .clone()
                .evaluate(host.repo.as_ref())
                .map_err(|error| error.to_string())?;
            pollster::block_on(revset.containing_fn()(host.commit.id()))
                .map(QueryValue::from)
                .map_err(|error| error.to_string())
        })
    })
}

fn native_jj_empty<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::empty", |host| {
        cached_semantic(host, "jj::empty", || {
            pollster::block_on(host.commit.is_empty(host.repo.as_ref()))
                .map(QueryValue::from)
                .map_err(|error| error.to_string())
        })
    })
}

fn native_jj_signature_present<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::signature_present", |host| {
        cached_semantic(host, "jj::signature_present", || {
            Ok(host.commit.is_signed().into())
        })
    })
}

fn native_jj_verify_signature<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::verify_signature", |host| {
        cached_semantic(host, "jj::verify_signature", || {
            match host.commit.verification() {
                Ok(None) => Ok(QueryValue::Null),
                Ok(Some(verification)) => Ok(object([
                    ("status", verification.status.to_string().into()),
                    (
                        "key",
                        verification
                            .key
                            .map(QueryValue::from)
                            .unwrap_or(QueryValue::Null),
                    ),
                    (
                        "display",
                        verification
                            .display
                            .map(QueryValue::from)
                            .unwrap_or(QueryValue::Null),
                    ),
                ])),
                Err(jj_lib::signing::SignError::InvalidSignatureFormat) => Ok(object([
                    ("status", "invalid".into()),
                    ("key", QueryValue::Null),
                    ("display", QueryValue::Null),
                ])),
                Err(error) => Err(error.to_string()),
            }
        })
    })
}

fn tree_side(path: &str, value: &MergedTreeValue) -> QueryValue {
    if value.is_absent() {
        return QueryValue::Null;
    }
    let simplified = value.simplify();
    let conflict = !simplified.is_resolved();
    let (file_type, executable) = if conflict {
        ("conflict", QueryValue::Null)
    } else {
        match simplified.as_resolved().and_then(Option::as_ref) {
            Some(TreeValue::File { executable, .. }) => ("file", QueryValue::from(*executable)),
            Some(TreeValue::Symlink(_)) => ("symlink", QueryValue::Null),
            Some(TreeValue::Tree(_)) => ("tree", QueryValue::Null),
            Some(TreeValue::GitSubmodule(_)) => ("git-submodule", QueryValue::Null),
            None => unreachable!("non-absent resolved tree value"),
        }
    };
    object([
        ("path", path.into()),
        ("file_type", file_type.into()),
        ("executable", executable),
        ("conflict", conflict.into()),
        ("conflict_side_count", simplified.num_sides().into()),
    ])
}

fn native_jj_diff_files<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::diff_files", |host| {
        cached_semantic(host, "jj::diff_files", || {
            pollster::block_on(async {
                let before = host.commit.parent_tree(host.repo.as_ref()).await?;
                let after = host.commit.tree();
                let mut stream = before.diff_stream(&after, host.matcher.as_ref());
                let mut entries = Vec::new();
                while let Some(entry) = stream.next().await {
                    let path = entry.path.as_internal_file_string().to_owned();
                    let values = entry.values?;
                    let status = if values.before.is_absent() {
                        "added"
                    } else if values.after.is_absent() {
                        "removed"
                    } else {
                        "modified"
                    };
                    entries.push(object([
                        ("path", path.clone().into()),
                        ("status", status.into()),
                        ("source", tree_side(&path, &values.before)),
                        ("target", tree_side(&path, &values.after)),
                    ]));
                }
                Ok::<_, jj_lib::backend::BackendError>(entries)
            })
            .map(|result| result.into_iter().collect())
            .map_err(|error| error.to_string())
        })
    })
}

fn native_jj_diff_stats<'a>(cv: jaq_core::Cv<'a, QueryData>) -> ValXs<'a, QueryValue> {
    with_commit(cv.1, "jj::diff_stats", |host| {
        cached_semantic(host, "jj::diff_stats", || {
            pollster::block_on(async {
                let before = host.commit.parent_tree(host.repo.as_ref()).await?;
                let after = host.commit.tree();
                let copy_records = CopyRecords::default();
                let tree_diff =
                    before.diff_stream_with_copies(&after, host.matcher.as_ref(), &copy_records);
                DiffStats::calculate(
                    host.repo.store(),
                    tree_diff,
                    &host.diff_stat_options,
                    host.conflict_marker_style,
                )
                .await
            })
            .map(|stats| {
                let files = stats
                    .entries()
                    .iter()
                    .map(|entry| {
                        let (lines_added, lines_removed) = entry
                            .added_removed
                            .map_or((QueryValue::Null, QueryValue::Null), |(added, removed)| {
                                (added.into(), removed.into())
                            });
                        object([
                            (
                                "path",
                                entry
                                    .path
                                    .target
                                    .as_internal_file_string()
                                    .to_owned()
                                    .into(),
                            ),
                            ("status", entry.status.label().into()),
                            ("lines_added", lines_added),
                            ("lines_removed", lines_removed),
                            ("bytes_delta", entry.bytes_delta.into()),
                        ])
                    })
                    .collect();
                object([
                    ("files", QueryValue::Array(Rc::new(files))),
                    ("total_added", stats.count_total_added().into()),
                    ("total_removed", stats.count_total_removed().into()),
                ])
            })
            .map_err(|error| error.to_string())
        })
    })
}

fn validate_source(source: &str) -> Result<(), String> {
    use jaq_core::load::parse::BinaryOp;
    use jaq_core::load::parse::Pattern;
    use jaq_core::load::parse::Term;

    let term = jaq_core::load::parse(source, |parser| parser.term())
        .ok_or_else(|| "query syntax is not valid for query version v1".to_owned())?;

    fn public_arity(name: &str, arity: usize) -> bool {
        let arities: &[usize] = match name {
            "true" | "false" | "null" => &[0],
            "empty" | "type" | "values" | "nulls" | "booleans" | "numbers" | "strings"
            | "arrays" | "objects" | "iterables" | "scalars" | "length" | "keys"
            | "keys_unsorted" | "to_entries" | "from_entries" | "flatten" | "reverse" | "sort"
            | "unique" | "min" | "max" | "ascii_downcase" | "ascii_upcase" | "explode"
            | "tostring" | "tonumber" | "tojson" | "fromjson" => &[0],
            "has" | "contains" | "inside" | "indices" | "index" | "rindex" | "select" | "map"
            | "map_values" | "with_entries" | "sort_by" | "group_by" | "unique_by" | "min_by"
            | "max_by" | "startswith" | "endswith" | "ltrimstr" | "rtrimstr" | "split" | "join"
            | "walk" => &[1],
            "range" => &[1, 2, 3],
            "add" => &[0, 1],
            "first" | "last" => &[0, 1],
            "nth" => &[1, 2],
            "limit" | "skip" | "until" | "while" => &[2],
            "any" | "all" => &[0, 1, 2],
            "recurse" => &[0, 1],
            "jj::mine"
            | "jj::working_copies"
            | "jj::current_working_copy"
            | "jj::bookmarks"
            | "jj::tags"
            | "jj::divergent"
            | "jj::hidden"
            | "jj::change_offset"
            | "jj::immutable"
            | "jj::empty"
            | "jj::signature_present"
            | "jj::verify_signature"
            | "jj::diff_files"
            | "jj::diff_stats" => &[0],
            _ => &[],
        };
        arities.contains(&arity)
    }

    fn visit<'s>(term: &Term<&'s str>, inherited_defs: &[(&'s str, usize)]) -> Result<(), String> {
        use jaq_core::load::lex::StrPart;
        match term {
            Term::Id | Term::Recurse | Term::Num(_) | Term::Var(_) => Ok(()),
            Term::Str(format, parts) => {
                if let Some(format) = format
                    && !matches!(*format, "@json" | "@base64" | "@uri" | "@csv" | "@tsv")
                {
                    return Err(format!(
                        "format {format} is not supported by query version v1"
                    ));
                }
                for part in parts {
                    if let StrPart::Term(term) = part {
                        visit(term, inherited_defs)?;
                    }
                }
                Ok(())
            }
            Term::Arr(term) => term
                .as_deref()
                .map(|term| visit(term, inherited_defs))
                .transpose()
                .map(|_| ()),
            Term::Obj(entries) => {
                for (key, value) in entries {
                    visit(key, inherited_defs)?;
                    if let Some(value) = value {
                        visit(value, inherited_defs)?;
                    }
                }
                Ok(())
            }
            Term::Neg(term) => visit(term, inherited_defs),
            Term::BinOp(left, operator, right) => {
                if let BinaryOp::Pipe(Some(pattern)) = operator
                    && !matches!(pattern, Pattern::Var(_))
                {
                    return Err(
                        "destructuring bindings are not supported by query version v1".to_owned(),
                    );
                }
                visit(left, inherited_defs)?;
                visit(right, inherited_defs)
            }
            Term::Label(_, _) | Term::Break(_) => {
                Err("label and break are not supported by query version v1".to_owned())
            }
            Term::Fold(_, generator, pattern, terms) => {
                if !matches!(pattern, Pattern::Var(_)) {
                    return Err(
                        "destructuring bindings are not supported by query version v1".to_owned(),
                    );
                }
                visit(generator, inherited_defs)?;
                for term in terms {
                    visit(term, inherited_defs)?;
                }
                Ok(())
            }
            Term::TryCatch(term, catch) => {
                visit(term, inherited_defs)?;
                catch
                    .as_deref()
                    .map(|term| visit(term, inherited_defs))
                    .transpose()
                    .map(|_| ())
            }
            Term::IfThenElse(branches, fallback) => {
                for (condition, result) in branches {
                    visit(condition, inherited_defs)?;
                    visit(result, inherited_defs)?;
                }
                fallback
                    .as_deref()
                    .map(|term| visit(term, inherited_defs))
                    .transpose()
                    .map(|_| ())
            }
            Term::Def(definitions, body) => {
                let mut defs = inherited_defs.to_vec();
                for definition in definitions {
                    if definition.name.contains("::") || definition.name.starts_with("__jj_") {
                        return Err(format!(
                            "definition name {:?} is reserved by query version v1",
                            definition.name
                        ));
                    }
                    defs.push((definition.name, definition.args.len()));
                }
                for definition in definitions {
                    visit(&definition.body, &defs)?;
                }
                visit(body, &defs)
            }
            Term::Call(name, arguments) => {
                let arity = arguments.len();
                if !public_arity(name, arity) && !inherited_defs.contains(&(*name, arity)) {
                    return Err(format!(
                        "filter {name}/{arity} is not supported by query version v1"
                    ));
                }
                for argument in arguments {
                    visit(argument, inherited_defs)?;
                }
                Ok(())
            }
            Term::Path(base, path) => {
                visit(base, inherited_defs)?;
                for (part, _) in &path.0 {
                    match part {
                        jaq_core::path::Part::Index(index) => visit(index, inherited_defs)?,
                        jaq_core::path::Part::Range(start, end) => {
                            if let Some(start) = start {
                                visit(start, inherited_defs)?;
                            }
                            if let Some(end) = end {
                                visit(end, inherited_defs)?;
                            }
                        }
                    }
                }
                Ok(())
            }
        }
    }

    visit(&term, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_query() {
        let program = QueryProgram::compile("{answer: 40 + 2}").unwrap();
        let output = program
            .run(QueryValue::Null)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(output[0].to_json_record().unwrap(), br#"{"answer":42}"#);
    }

    #[test]
    fn test_query_v1_validation() {
        assert!(QueryProgram::compile("input").is_err());
        assert!(QueryProgram::compile("implode").is_err());
        assert!(QueryProgram::compile("label $done | break $done").is_err());
        assert!(QueryProgram::compile(". as [$first] | $first").is_err());
        assert!(QueryProgram::compile("import \"other\" as other; .").is_err());
        assert!(QueryProgram::compile("[1, 2] | add(.)").is_ok());
        assert!(QueryProgram::compile(r#""import include module""#).is_ok());

        let program = QueryProgram::compile("def double: . * 2; 21 | double").unwrap();
        let output = program
            .run(QueryValue::Null)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(output[0].to_json_record().unwrap(), b"42");
    }

    #[test]
    fn test_v1_manifest_is_valid_toml() {
        let manifest: toml::Value = toml::from_str(include_str!("v1-builtins.toml")).unwrap();
        assert!(manifest["jq"].as_array().unwrap().len() > 50);
        assert_eq!(manifest["jj"].as_array().unwrap().len(), 13);
    }
}
