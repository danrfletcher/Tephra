//! Google Drive remote sources (PR-10): `source.add_drive`, the first index
//! and the pure rules the changes-feed poller shares with them
//! (`remote/poll/drive.rs`).
//!
//! A Drive source is ONE picked folder: a path below My Drive, a folder id
//! or a folder URL. Never the whole Drive (F4), never a folder of a Shared
//! Drive (F7). The add runs in two steps, like a GitHub source:
//!
//! 1. Inside the call: the checks that touch no network (name, folder
//!    text), the credential, who the token is, the folder, the changes-feed
//!    start token, and then the whole tree below the folder (metadata
//!    only). The start token is STORED, with the new source, before the
//!    first listing page is asked for, so an edit made while the tree is
//!    listed or indexed is in the feed from there on, even if the daemon
//!    stops part-way. When the listing is done every file is stored in state
//!    `remote` and the call returns within seconds. A refusal at any point
//!    takes the half-made source away again, changes nothing else and
//!    announces nothing. A start-up that finds a source cut off while it was
//!    listed lists it again from scratch with a fresh token.
//! 2. In the background: each text file is read ONCE (a Markdown, text,
//!    code or document file by media download, a native Google Doc through
//!    the `text/markdown` export), parsed by the same code a local file goes
//!    through, and the bytes are dropped. Sheets, Slides, Forms, Drawings,
//!    shortcuts, other Google files, binaries and files over
//!    [`MAX_INDEXED_BYTES`] are listed by name only and never read (F5).
//!
//! Every path is tied to the Drive file id: a rename is the same file with a
//! new path, and two files that would share a path (the same name, or names
//! that differ only by case, in one folder) get `name (2)` for the later one
//! (by created time, then id) and keep it (E9). A name is mapped to a safe
//! path segment ([`safe_segment`]), so `/`, `..`, control characters and
//! over-long names never leave the source root (E10).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, UNIX_EPOCH};

use tephra_cache::{
    encode_cursor, NewRemoteItem, NewRemoteSource, RemoteItemRow, RemoteSourceRow, ITEM_KIND_GDOC,
    ITEM_KIND_GDRAWING, ITEM_KIND_GFORM, ITEM_KIND_GSHEET, ITEM_KIND_GSHORTCUT, ITEM_KIND_GSLIDES,
    STATUS_ACTIVE, STATUS_AUTH_REQUIRED, STATUS_INDEXING, STATUS_OFFLINE, STATUS_SYNCING,
};
use tephra_remote::credentials::{credential_state, CredentialState, Provider};
use tephra_remote::drive::{DriveAdapter, DriveFile, DriveUser, GOOGLE_DOC_MIME};
use tephra_remote::{RemoteError, UnreachableCause};

use crate::file_kind::{kind_from_name, MAX_INDEXED_BYTES};
use crate::registry::{
    check_source_name, retry_cache_on_busy, RegistryError, SourceChange, SourceChangeKind,
    SourceRegistry,
};
use crate::remote::safe_path::safe_segment;
use crate::remote_github::{hhmm_utc, now_secs, printable, refused, IndexJob, DRIVE_KIND};

const SHEET_MIME: &str = "application/vnd.google-apps.spreadsheet";
const SLIDES_MIME: &str = "application/vnd.google-apps.presentation";
const FORM_MIME: &str = "application/vnd.google-apps.form";
const DRAWING_MIME: &str = "application/vnd.google-apps.drawing";
const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";
const GOOGLE_APPS_PREFIX: &str = "application/vnd.google-apps.";

/// Consecutive files whose read failed with a server error before the pass
/// stops and the source is `offline` (one bad file never stops the rest).
const CONSECUTIVE_FAILS: u32 = 3;

/// Tries one file's read gets for a connection that dropped.
const READ_TRIES: u32 = 3;

/// The longest the indexer sleeps at a time while a rate limit holds.
const RATE_LIMIT_SLICE: Duration = Duration::from_secs(1);

/// The reason of a file whose read was refused (the token may not read it).
pub(crate) const REASON_PERMISSION_DENIED: &str = "permission_denied";
/// The reason of a file Drive no longer has.
pub(crate) const REASON_NOT_FOUND: &str = "not_found";
/// The reason of a file whose bytes turned out over the index cap.
pub(crate) const REASON_TOO_LARGE: &str = "too_large";

// ---------------------------------------------------------------------------
// The listing, as items.
// ---------------------------------------------------------------------------

/// One file of a Drive folder tree, as the index lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriveItem {
    pub path: String,
    pub kind: &'static str,
    pub size: Option<i64>,
    pub file_id: String,
    pub revision: Option<String>,
}

impl DriveItem {
    pub(crate) fn as_new(&self) -> NewRemoteItem<'_> {
        NewRemoteItem {
            path: &self.path,
            kind: self.kind,
            sha: "",
            size: self.size,
            file_id: Some(&self.file_id),
            revision: self.revision.as_deref(),
        }
    }
}

/// A file of the tree and the folder it is in: `dir` is the path of that
/// folder below the source root, safe segments each ending in `/` (empty at
/// the root).
#[derive(Debug, Clone)]
pub(crate) struct Listed {
    pub file: DriveFile,
    pub dir: String,
}

/// The kind a listing gives `file` and the file name its path is made from.
/// A native Doc is read as Markdown, so its path ends in `.md`; every other
/// Google file keeps its name and is never read.
fn kind_and_name(file: &DriveFile) -> (&'static str, String) {
    match file.mime_type.as_str() {
        GOOGLE_DOC_MIME => {
            let name = if file.name.to_lowercase().ends_with(".md") {
                file.name.clone()
            } else {
                format!("{}.md", file.name)
            };
            (ITEM_KIND_GDOC, name)
        }
        SHEET_MIME => (ITEM_KIND_GSHEET, file.name.clone()),
        SLIDES_MIME => (ITEM_KIND_GSLIDES, file.name.clone()),
        FORM_MIME => (ITEM_KIND_GFORM, file.name.clone()),
        DRAWING_MIME => (ITEM_KIND_GDRAWING, file.name.clone()),
        SHORTCUT_MIME => (ITEM_KIND_GSHORTCUT, file.name.clone()),
        other if other.starts_with(GOOGLE_APPS_PREFIX) => ("binary", file.name.clone()),
        _ => (kind_from_name(&file.name).as_str(), file.name.clone()),
    }
}

/// Splits a file name into the part before its extension and the extension
/// with its dot (empty when there is none).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at)
            if at > 0
                && at + 1 < name.len()
                && name.len() - at <= 9
                && name[at + 1..].chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            name.split_at(at)
        }
        _ => (name, ""),
    }
}

/// `Plan.md` as the `n`th file of that name: `Plan (2).md` (the suffix goes
/// before the extension). `n` of 1 is the name itself.
fn with_suffix(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_string();
    }
    let (stem, ext) = split_ext(name);
    format!("{stem} ({n}){ext}")
}

/// The safe path of the `n`th file called `name` in `dir`.
fn candidate(dir: &str, name: &str, n: u32) -> String {
    format!("{dir}{}", safe_segment(&with_suffix(name, n)))
}

/// Whether `path` (what the file had) is still the path of a file called
/// `name` in `dir`: the plain name, or `name (k)` for a `k` of 2 or more.
fn still_its_path(path: &str, dir: &str, name: &str) -> bool {
    let Some(last) = path.strip_prefix(dir) else {
        return false;
    };
    if last.contains('/') {
        return false;
    }
    if path == candidate(dir, name, 1) {
        return true;
    }
    // `... (k)` before the extension.
    let (stem, _) = split_ext(last);
    let Some(open) = stem.rfind(" (") else {
        return false;
    };
    let Some(number) = stem[open + 2..].strip_suffix(')') else {
        return false;
    };
    match number.parse::<u32>() {
        Ok(k) if k >= 2 => path == candidate(dir, name, k),
        _ => false,
    }
}

/// The path of each file of `listed`, in the order given.
///
/// A file that already had a path keeps it while that path is still its
/// name (or its name with the `(k)` it was given), so a path is stable:
/// across a restart, a re-list, and the deletion of the file that came
/// first. The rest are given the plain name, or `name (2)`, `name (3)`...,
/// in the order of their created time and then their id, skipping every path
/// in use. Paths are compared without regard to case, so two names that
/// differ only by case do not share one. `occupied` holds the (lowercase)
/// paths of files that are not in `listed` and keep theirs.
pub(crate) fn assign_paths(
    listed: &[Listed],
    existing: &HashMap<String, String>,
    occupied: &HashSet<String>,
) -> Vec<String> {
    let mut taken: HashSet<String> = occupied.clone();
    let mut paths: Vec<Option<String>> = vec![None; listed.len()];
    let names: Vec<String> = listed.iter().map(|l| kind_and_name(&l.file).1).collect();

    // First, the files that keep the path they had.
    for (i, l) in listed.iter().enumerate() {
        let Some(had) = existing.get(&l.file.id) else {
            continue;
        };
        if still_its_path(had, &l.dir, &names[i]) && taken.insert(had.to_lowercase()) {
            paths[i] = Some(had.clone());
        }
    }
    // Then the others, earliest created first.
    let mut rest: Vec<usize> = (0..listed.len()).filter(|i| paths[*i].is_none()).collect();
    rest.sort_by(|a, b| {
        let key = |i: &usize| {
            (
                listed[*i].file.created_time.clone().unwrap_or_default(),
                listed[*i].file.id.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
    for i in rest {
        let mut n = 1;
        loop {
            let path = candidate(&listed[i].dir, &names[i], n);
            if taken.insert(path.to_lowercase()) {
                paths[i] = Some(path);
                break;
            }
            n += 1;
        }
    }
    paths.into_iter().map(|p| p.unwrap_or_default()).collect()
}

/// The items of `listed` at the paths `paths` (see [`assign_paths`]).
pub(crate) fn build_items(listed: &[Listed], paths: Vec<String>) -> Vec<DriveItem> {
    listed
        .iter()
        .zip(paths)
        .map(|(l, path)| {
            let (kind, _) = kind_and_name(&l.file);
            DriveItem {
                path,
                kind,
                size: l.file.size.map(|s| s as i64),
                file_id: l.file.id.clone(),
                revision: l.file.revision().map(str::to_string),
            }
        })
        .collect()
}

/// The folder-and-file paths already in use, lowercase, except those of the
/// files in `except` (by Drive file id).
pub(crate) fn occupied_paths(rows: &[RemoteItemRow], except: &HashSet<&str>) -> HashSet<String> {
    rows.iter()
        .filter(|r| !r.file_id.as_deref().is_some_and(|id| except.contains(id)))
        .map(|r| r.path.to_lowercase())
        .collect()
}

/// The path each file id has in `rows`.
pub(crate) fn paths_by_file_id(rows: &[RemoteItemRow]) -> HashMap<String, String> {
    rows.iter()
        .filter_map(|r| r.file_id.clone().map(|id| (id, r.path.clone())))
        .collect()
}

/// Every file below the folder `root`, metadata only: no content is read.
/// Folders are walked breadth first, each once (a cycle ends); a shortcut is
/// listed as itself and never followed; a trashed file is not listed. A file
/// with several parents is listed once, in the first of its `parents` that
/// is inside the tree (so the changes feed, which reads `parents` the same
/// way, agrees with it). Folders are not items.
pub(crate) async fn list_tree(adapter: &dyn DriveAdapter, root: &str) -> Result<Tree, RemoteError> {
    // Folder id -> its path below the root (safe segments, each ending `/`).
    let mut dirs: HashMap<String, String> = HashMap::new();
    dirs.insert(root.to_string(), String::new());
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(root.to_string());
    // Every file seen, in the order met, with the folder it was met in.
    let mut seen_files: Vec<(DriveFile, String)> = Vec::new();
    let mut file_ids: HashSet<String> = HashSet::new();
    while let Some(folder) = queue.pop_front() {
        let dir = dirs.get(&folder).cloned().unwrap_or_default();
        let mut page_token: Option<String> = None;
        loop {
            let page = adapter
                .list_children(&folder, page_token.as_deref())
                .await?;
            for child in page.files {
                if child.trashed {
                    continue;
                }
                if child.is_folder() {
                    if !dirs.contains_key(&child.id) {
                        dirs.insert(
                            child.id.clone(),
                            format!("{dir}{}/", safe_segment(&child.name)),
                        );
                        queue.push_back(child.id);
                    }
                } else if file_ids.insert(child.id.clone()) {
                    seen_files.push((child, folder.clone()));
                }
            }
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
    }
    let files = seen_files
        .into_iter()
        .map(|(file, met_in)| {
            let home = file
                .parents
                .iter()
                .find(|p| dirs.contains_key(*p))
                .cloned()
                .unwrap_or(met_in);
            let dir = dirs.get(&home).cloned().unwrap_or_default();
            Listed { file, dir }
        })
        .collect();
    Ok(Tree {
        files,
        folders: dirs,
    })
}

/// What [`list_tree`] found.
pub(crate) struct Tree {
    pub files: Vec<Listed>,
    /// Every folder of the tree (the root too, as ""), by id: its path
    /// below the root, safe segments each ending in `/`.
    pub folders: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// The folder the person picked.
// ---------------------------------------------------------------------------

/// What `source add drive` was given as the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FolderSpec {
    /// A folder id (from an id or a URL).
    Id(String),
    /// A path below My Drive.
    Path(Vec<String>),
    /// One word: a folder id, or the name of a folder in My Drive.
    Bare(String),
}

fn is_id_text(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 256
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The folder id in a Drive URL, if it has one:
/// `https://drive.google.com/drive/folders/<id>` (with or without
/// `/u/<n>/` and a query) or `https://drive.google.com/open?id=<id>`.
fn id_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if host != "drive.google.com" && host != "docs.google.com" {
        return None;
    }
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, q),
        None => (tail, ""),
    };
    let path = path.split('#').next().unwrap_or("");
    let query = query.split('#').next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').collect();
    if let Some(at) = segments.iter().position(|s| *s == "folders") {
        if let Some(id) = segments.get(at + 1).filter(|id| is_id_text(id)) {
            return Some((*id).to_string());
        }
    }
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, v)| *k == "id" && is_id_text(v))
        .map(|(_, v)| v.to_string())
}

/// The folder spec of `text`, or why it is refused. Touches no network.
pub(crate) fn parse_folder(text: &str) -> Result<FolderSpec, RegistryError> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '/') {
        return Err(refused(
            "whole_drive",
            "a Drive source is one folder: name a folder (a path, an id or a URL); the whole \
             Drive cannot be added",
        ));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(refused(
            "invalid_folder",
            "the folder text has a control character in it",
        ));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return match id_from_url(trimmed) {
            Some(id) => Ok(FolderSpec::Id(id)),
            None => Err(refused(
                "invalid_folder",
                format!(
                    "\"{}\" is not the URL of a Drive folder (expected \
                     https://drive.google.com/drive/folders/<id>)",
                    printable(trimmed)
                ),
            )),
        };
    }
    if trimmed.contains('/') {
        let mut segments = Vec::new();
        for part in trimmed.trim_matches('/').split('/') {
            if part.is_empty() || part == "." || part == ".." {
                return Err(refused(
                    "invalid_folder",
                    format!(
                        "\"{}\" is not a folder path (an empty, \".\" or \"..\" part)",
                        printable(trimmed)
                    ),
                ));
            }
            segments.push(part.to_string());
        }
        return Ok(FolderSpec::Path(segments));
    }
    if trimmed == "." || trimmed == ".." {
        return Err(refused(
            "invalid_folder",
            "\".\" and \"..\" are not folders",
        ));
    }
    Ok(FolderSpec::Bare(trimmed.to_string()))
}

/// What `source.add_drive` answers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DriveAddWire {
    pub name: String,
    pub kind: &'static str,
    /// The folder's id: what the source is tied to.
    pub folder: String,
    pub folder_name: String,
    /// Files in the listing.
    pub files: usize,
    pub status: String,
    pub index_done: usize,
    pub index_total: usize,
}

/// The refusal for an adapter error met while checking the credential, the
/// folder or the listing. None of the messages holds a token.
fn refuse_drive(e: RemoteError) -> RegistryError {
    match e {
        RemoteError::AuthRejected => refused(
            "auth_required",
            "Google rejected the token (it may be revoked or expired): run `tephractl auth google` \
             and try again",
        ),
        RemoteError::PermissionDenied => refused(
            "permission_denied",
            "the token is not allowed to read that folder",
        ),
        RemoteError::NotFound => refused(
            "not_found",
            "no such folder in your Drive (a folder in a Shared Drive, or one shared with you \
             and not added to My Drive, cannot be added)",
        ),
        RemoteError::InvalidInput(_) => refused("invalid_folder", "that is not a Drive folder id"),
        RemoteError::RateLimited { retry_at } => {
            let secs = retry_at
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            refused(
                "rate_limited",
                format!(
                    "Google Drive is rate limiting this token: try again after {}",
                    hhmm_utc(secs)
                ),
            )
        }
        other => RegistryError::Remote(crate::remote_github::RemoteAddError::Failed(format!(
            "could not reach Google Drive: {other}"
        ))),
    }
}

/// The folder `spec` names, checked: a folder (not a file) in My Drive that
/// is not in the trash and is not the Drive's own root.
async fn resolve_folder(
    adapter: &dyn DriveAdapter,
    spec: &FolderSpec,
) -> Result<DriveFile, RegistryError> {
    let file = match spec {
        FolderSpec::Id(id) => adapter.get_file(id).await.map_err(refuse_drive)?,
        FolderSpec::Bare(word) => {
            let by_id = if is_id_text(word) {
                match adapter.get_file(word).await {
                    Ok(file) => Some(file),
                    Err(RemoteError::NotFound | RemoteError::InvalidInput(_)) => None,
                    Err(e) => return Err(refuse_drive(e)),
                }
            } else {
                None
            };
            match by_id {
                Some(file) => file,
                None => walk_path(adapter, std::slice::from_ref(word)).await?,
            }
        }
        FolderSpec::Path(segments) => walk_path(adapter, segments).await?,
    };
    if !file.is_folder() {
        return Err(refused(
            "not_a_folder",
            format!("\"{}\" is a file, not a folder", printable(&file.name)),
        ));
    }
    if file.trashed {
        return Err(refused(
            "not_found",
            format!("the folder \"{}\" is in the trash", printable(&file.name)),
        ));
    }
    if file.drive_id.is_some() {
        return Err(refused(
            "shared_drive",
            format!(
                "\"{}\" is in a Shared Drive: Shared Drives are not supported, pick a folder in \
                 My Drive",
                printable(&file.name)
            ),
        ));
    }
    if file.parents.is_empty() {
        return Err(refused(
            "whole_drive",
            "that is the root of the Drive: a Drive source is one folder, not the whole Drive",
        ));
    }
    Ok(file)
}

/// The folder at `segments` below My Drive. A name that two folders share is
/// refused, so the source is never one the person did not mean.
async fn walk_path(
    adapter: &dyn DriveAdapter,
    segments: &[String],
) -> Result<DriveFile, RegistryError> {
    let mut parent = "root".to_string();
    let mut found: Option<DriveFile> = None;
    for segment in segments {
        let mut matches: Vec<DriveFile> = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let page = adapter
                .list_children(&parent, page_token.as_deref())
                .await
                .map_err(refuse_drive)?;
            matches.extend(
                page.files
                    .into_iter()
                    .filter(|f| f.is_folder() && !f.trashed && f.name == *segment),
            );
            match page.next_page_token {
                Some(next) => page_token = Some(next),
                None => break,
            }
        }
        match matches.len() {
            0 => {
                return Err(refused(
                    "not_found",
                    format!(
                        "no folder named \"{}\" in {}",
                        printable(segment),
                        if found.is_none() {
                            "My Drive".to_string()
                        } else {
                            format!("\"{}\"", printable(&parent))
                        }
                    ),
                ))
            }
            1 => {
                let folder = matches.remove(0);
                parent = folder.id.clone();
                found = Some(folder);
            }
            _ => {
                return Err(refused(
                    "ambiguous_folder",
                    format!(
                        "more than one folder is named \"{}\": give its id or URL instead",
                        printable(segment)
                    ),
                ))
            }
        }
    }
    found.ok_or_else(|| refused("whole_drive", "a Drive source is one folder"))
}

impl SourceRegistry {
    /// Takes away the half-added source of a Drive add that is refused
    /// after its feed position was stored (see [`SourceRegistry::register_drive`]).
    /// Nothing was announced for it. A failure here is logged: start-up
    /// lists such a source again from scratch.
    fn abandon_drive_source(&self, name: &str) {
        let cache = self.cache.lock().unwrap();
        if let Err(e) = retry_cache_on_busy(&self.gate, || cache.abandon_remote_source(name)) {
            tracing::warn!(source = %name, error = %e, "could not take away a refused Drive source");
        }
    }

    /// `source.add_drive`: registers ONE picked folder and takes its listing
    /// (see the module doc). Blocking: call it from a blocking thread.
    pub fn register_drive(
        &self,
        name: &str,
        folder_text: &str,
    ) -> Result<DriveAddWire, RegistryError> {
        // 1. What needs no network.
        check_source_name(name).map_err(|why| {
            refused(
                "invalid_name",
                format!("\"{}\" is not a usable source name: {why}", printable(name)),
            )
        })?;
        let spec = parse_folder(folder_text)?;
        let lowered = name.to_lowercase();
        {
            let cache = self.cache.lock().unwrap();
            if cache
                .list_sources()?
                .iter()
                .any(|r| r.name.to_lowercase() == lowered)
            {
                return Err(refused(
                    "name_taken",
                    format!("a source named \"{name}\" (or differing only in case) already exists"),
                ));
            }
        }
        let Some(hub) = self.remote.get().cloned() else {
            return Err(refused(
                "auth_required",
                "this daemon has no credential store, so no Drive source can be added",
            ));
        };
        match credential_state(&*hub.store, Provider::Google) {
            CredentialState::SignedIn => {}
            CredentialState::SignedOut => {
                return Err(refused(
                    "auth_required",
                    "no Google sign-in is stored: run `tephractl auth google` first",
                ))
            }
            CredentialState::AuthRequired(reason) => {
                return Err(refused(
                    "auth_required",
                    format!("the Google sign-in cannot be read: {reason}"),
                ))
            }
        }

        // 2. Who the token is, the folder, then the start token.
        let adapter = hub.drive.clone();
        let (folder, token) = self.runtime.block_on(async {
            adapter
                .whoami()
                .await
                .map(|_: DriveUser| ())
                .map_err(refuse_drive)?;
            let folder = resolve_folder(&*adapter, &spec).await?;
            let token = adapter.start_page_token().await.map_err(refuse_drive)?;
            Ok::<_, RegistryError>((folder, token))
        })?;

        // 3. The start token is STORED before the first listing page is
        // asked for: what changes while the tree is listed and indexed is
        // in the feed from this token on, even if the daemon stops
        // part-way. The source exists from here (`syncing`, listing the
        // folder); a refusal below takes it away again, so a refused add
        // still leaves nothing behind and announces nothing.
        let now = now_secs(&*hub.clock);
        let cursor = encode_cursor(&token, None);
        {
            let cache = self.cache.lock().unwrap();
            let new = NewRemoteSource {
                name,
                kind: DRIVE_KIND,
                target: &folder.id,
                branch: None,
                cursor: &cursor,
                now,
            };
            retry_cache_on_busy(&self.gate, || cache.begin_remote_source(&new))?;
        }
        let listed = self
            .runtime
            .block_on(list_tree(&*adapter, &folder.id))
            .map_err(refuse_drive);
        let tree = match listed {
            Ok(tree) => tree.files,
            Err(refusal) => {
                self.abandon_drive_source(name);
                return Err(refusal);
            }
        };

        // 4. The listing, as rows.
        let paths = assign_paths(&tree, &HashMap::new(), &HashSet::new());
        let listed = build_items(&tree, paths);
        drop(tree);
        let items: Vec<NewRemoteItem<'_>> = listed.iter().map(DriveItem::as_new).collect();
        let files = items.len();
        {
            let cache = self.cache.lock().unwrap();
            let filled = retry_cache_on_busy(&self.gate, || {
                cache.fill_remote_source(name, &items, MAX_INDEXED_BYTES)
            });
            if let Err(e) = filled {
                drop(cache);
                self.abandon_drive_source(name);
                return Err(e.into());
            }
        }
        tracing::info!(source = %name, files, "drive source added");

        // 5. Tell the clients, then read the text in the background.
        self.announce_sources_changed(vec![SourceChange::new(SourceChangeKind::Added, name)]);
        self.spawn_remote_index(name);
        self.spawn_remote_poll(name);
        let progress = {
            let cache = self.cache.lock().unwrap();
            cache.remote_index_progress(name, MAX_INDEXED_BYTES)?
        };
        Ok(DriveAddWire {
            name: name.to_string(),
            kind: DRIVE_KIND,
            folder: folder.id,
            folder_name: folder.name,
            files,
            status: STATUS_INDEXING.to_string(),
            index_done: progress.done,
            index_total: progress.total,
        })
    }
}

// ---------------------------------------------------------------------------
// What a failed request means.
// ---------------------------------------------------------------------------

/// The reason an `offline` Drive source shows for `error`.
pub(crate) fn offline_reason(error: &RemoteError) -> String {
    match error {
        RemoteError::Unreachable(cause) => {
            format!("cannot reach Google Drive ({})", unreachable_text(*cause))
        }
        RemoteError::Server { status } => {
            format!("cannot reach Google Drive (it answered {status})")
        }
        other => format!("cannot reach Google Drive ({other})"),
    }
}

fn unreachable_text(cause: UnreachableCause) -> &'static str {
    cause.as_str()
}

/// The reason a rate-limited Drive source shows while it waits.
pub(crate) fn rate_limit_reason(what: &str, retry_at: std::time::SystemTime) -> String {
    let secs = retry_at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!(
        "rate limited by Google Drive; {what} resumes at {}",
        hhmm_utc(secs)
    )
}

/// The reason of a source whose token Google refused.
pub(crate) const REASON_TOKEN: &str = "Google rejected the token: run `tephractl auth google`";

// ---------------------------------------------------------------------------
// The first index.
// ---------------------------------------------------------------------------

/// Why one file's read stopped.
enum ReadStop {
    /// The file is gone (404).
    Gone,
    /// The token may not read the file (403 that is not a rate limit).
    Denied,
    AuthRejected,
    /// The connection is away: the pass stops, the source is `offline`.
    Offline(String),
    /// A server error for this file: it stays listed, the pass goes on.
    Failed(String),
    /// More than [`MAX_INDEXED_BYTES`] came.
    TooLarge,
    Shutdown,
}

/// A media download that is cut off one byte past [`MAX_INDEXED_BYTES`]: a
/// file that is bigger than its listing said is never held whole.
pub(crate) async fn download_capped(
    adapter: &dyn DriveAdapter,
    file_id: &str,
) -> Result<Vec<u8>, RemoteError> {
    let mut stream = adapter.stream_download(file_id).await?;
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > MAX_INDEXED_BYTES {
            break;
        }
    }
    Ok(bytes)
}

impl IndexJob {
    /// The Drive index: every text item with no content hash is read once
    /// and written. Returns `true` when the pass reached the end (the source
    /// is then `active`), `false` when it stopped (the failure is recorded on
    /// the source and the poller tries again).
    pub(crate) async fn index_drive(&self, source: &RemoteSourceRow) -> Result<bool, String> {
        if source.status == STATUS_OFFLINE || source.status == STATUS_AUTH_REQUIRED {
            self.reason_set
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let pending = self
            .db(|c, n| c.remote_pending_items(n, MAX_INDEXED_BYTES))
            .await?;
        let mut since_announce = 0usize;
        let mut failed_in_a_row = 0u32;
        for item in pending {
            let Some(file_id) = item.file_id.clone() else {
                continue;
            };
            let native = item.kind.as_deref() == Some(ITEM_KIND_GDOC);
            let path = item.path.clone();
            match self.read_drive(&file_id, native).await {
                Ok(bytes) => {
                    failed_in_a_row = 0;
                    // Parsed on the blocking pool, written, and dropped
                    // there.
                    self.write_drive_one(path, bytes).await?;
                    since_announce += 1;
                    if since_announce >= 200 {
                        since_announce = 0;
                        self.announce();
                    }
                }
                Err(ReadStop::Gone) => {
                    self.db(move |c, n| c.set_remote_item_reason(n, &path, Some(REASON_NOT_FOUND)))
                        .await?;
                }
                Err(ReadStop::Denied) => {
                    self.db(move |c, n| {
                        c.set_remote_item_reason(n, &path, Some(REASON_PERMISSION_DENIED))
                    })
                    .await?;
                }
                Err(ReadStop::TooLarge) => {
                    self.db(move |c, n| c.set_remote_item_reason(n, &path, Some(REASON_TOO_LARGE)))
                        .await?;
                }
                Err(ReadStop::AuthRejected) => {
                    self.stop_with(source, STATUS_AUTH_REQUIRED, REASON_TOKEN.to_string())
                        .await?;
                    return Ok(false);
                }
                Err(ReadStop::Offline(why)) => {
                    self.stop_with(source, STATUS_OFFLINE, why).await?;
                    return Ok(false);
                }
                // One file Drive would not give: it stays listed and a later
                // poll reads it again. The rest of the index goes on, unless
                // Drive fails for every file in a row.
                Err(ReadStop::Failed(why)) => {
                    tracing::warn!(source = %self.name, path = %path, "a Drive read failed: {why}");
                    failed_in_a_row += 1;
                    if failed_in_a_row >= CONSECUTIVE_FAILS {
                        self.stop_with(source, STATUS_OFFLINE, why).await?;
                        return Ok(false);
                    }
                }
                Err(ReadStop::Shutdown) => return Ok(false),
            }
        }
        let now = now_secs(&*self.hub.clock);
        self.db(move |c, n| {
            c.set_remote_index_complete(n, true, now)?;
            c.set_remote_status(n, STATUS_ACTIVE, None, Some(now))
        })
        .await?;
        self.announce();
        Ok(true)
    }

    /// Writes the rows of one file from its bytes, in one transaction.
    async fn write_drive_one(&self, path: String, bytes: Vec<u8>) -> Result<(), String> {
        self.db(move |cache, name| {
            let Some(row) = cache.get_source(name)? else {
                return Ok(());
            };
            let tx = cache.connection().unchecked_transaction()?;
            crate::scan::write_remote_unit(cache, row.id, &path, &bytes)
                .map_err(crate::scan::cache_error_of)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// The bytes of one file, with the waits a rate limit asks for and a few
    /// tries for a connection that drops. A native Doc is the Markdown
    /// export; any other file is read through the media download, stopping
    /// at [`MAX_INDEXED_BYTES`].
    async fn read_drive(&self, file_id: &str, native_doc: bool) -> Result<Vec<u8>, ReadStop> {
        let mut tries = 0;
        loop {
            let answer = if native_doc {
                self.hub.drive.export_markdown(file_id).await
            } else {
                self.download_capped(file_id).await
            };
            match answer {
                Ok(bytes) if bytes.len() as u64 > MAX_INDEXED_BYTES => {
                    return Err(ReadStop::TooLarge)
                }
                Ok(bytes) => {
                    if self
                        .reason_set
                        .swap(false, std::sync::atomic::Ordering::Relaxed)
                    {
                        let _ = self
                            .db(|c, n| c.set_remote_status(n, STATUS_INDEXING, None, None))
                            .await;
                    }
                    return Ok(bytes);
                }
                Err(RemoteError::RateLimited { retry_at }) => self.wait_out_drive(retry_at).await?,
                Err(RemoteError::NotFound) => return Err(ReadStop::Gone),
                Err(RemoteError::PermissionDenied) => return Err(ReadStop::Denied),
                Err(RemoteError::AuthRejected) => return Err(ReadStop::AuthRejected),
                Err(e @ RemoteError::Unreachable(_)) => {
                    tries += 1;
                    if tries >= READ_TRIES {
                        return Err(ReadStop::Offline(offline_reason(&e)));
                    }
                    tokio::time::sleep(Duration::from_millis(200 * u64::from(tries))).await;
                }
                Err(e @ RemoteError::Server { .. }) => {
                    tries += 1;
                    if tries >= READ_TRIES {
                        return Err(ReadStop::Failed(offline_reason(&e)));
                    }
                    tokio::time::sleep(Duration::from_millis(200 * u64::from(tries))).await;
                }
                Err(e) => {
                    return Err(ReadStop::Failed(format!(
                        "could not read from Google Drive: {e}"
                    )))
                }
            }
        }
    }

    /// A media download that is cut off one byte past the cap: a file that
    /// is bigger than its listing said is never held whole.
    async fn download_capped(&self, file_id: &str) -> Result<Vec<u8>, RemoteError> {
        download_capped(&*self.hub.drive, file_id).await
    }

    /// Sleeps until `retry_at`, noting it on the source as `syncing` with the
    /// time (E1). A source that is gone ends the wait.
    async fn wait_out_drive(&self, retry_at: std::time::SystemTime) -> Result<(), ReadStop> {
        let reason = rate_limit_reason("reading", retry_at);
        self.reason_set
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self
            .db(move |c, n| c.set_remote_status(n, STATUS_SYNCING, Some(&reason), None))
            .await;
        self.announce();
        loop {
            let left = retry_at
                .duration_since(self.hub.clock.now())
                .unwrap_or_default();
            if left.is_zero() {
                return Ok(());
            }
            if self.source_gone().await {
                return Err(ReadStop::Shutdown);
            }
            tokio::time::sleep(left.min(RATE_LIMIT_SLICE)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, name: &str, created: &str) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: name.to_string(),
            mime_type: "text/markdown".to_string(),
            parents: vec!["p".to_string()],
            head_revision_id: None,
            md5_checksum: None,
            version: None,
            created_time: Some(created.to_string()),
            trashed: false,
            drive_id: None,
            size: Some(1),
        }
    }

    fn listed(id: &str, name: &str, created: &str) -> Listed {
        Listed {
            file: file(id, name, created),
            dir: String::new(),
        }
    }

    fn paths_of(
        files: &[Listed],
        existing: &HashMap<String, String>,
        occupied: &HashSet<String>,
    ) -> Vec<String> {
        assign_paths(files, existing, occupied)
    }

    #[test]
    fn the_later_file_of_a_name_gets_the_suffix_before_the_extension() {
        let files = [
            listed("b", "Plan.md", "2026-01-02T00:00:00Z"),
            listed("a", "Plan.md", "2026-01-01T00:00:00Z"),
        ];
        let paths = paths_of(&files, &HashMap::new(), &HashSet::new());
        assert_eq!(paths, vec!["Plan (2).md", "Plan.md"]);
    }

    #[test]
    fn names_that_differ_only_by_case_do_not_share_a_path() {
        let files = [
            listed("a", "Plan.md", "2026-01-01T00:00:00Z"),
            listed("b", "plan.md", "2026-01-02T00:00:00Z"),
        ];
        let paths = paths_of(&files, &HashMap::new(), &HashSet::new());
        assert_eq!(paths, vec!["Plan.md", "plan (2).md"]);
    }

    #[test]
    fn created_time_ties_are_broken_by_file_id() {
        let files = [
            listed("z", "N.md", "2026-01-01T00:00:00Z"),
            listed("y", "N.md", "2026-01-01T00:00:00Z"),
        ];
        let paths = paths_of(&files, &HashMap::new(), &HashSet::new());
        assert_eq!(paths, vec!["N (2).md", "N.md"]);
    }

    #[test]
    fn a_file_keeps_its_suffix_when_the_earlier_one_is_gone() {
        let files = [listed("b", "Plan.md", "2026-01-02T00:00:00Z")];
        let existing: HashMap<String, String> =
            [("b".to_string(), "Plan (2).md".to_string())].into();
        let paths = paths_of(&files, &existing, &HashSet::new());
        assert_eq!(paths, vec!["Plan (2).md"]);
    }

    #[test]
    fn a_renamed_file_gets_a_path_for_its_new_name() {
        let files = [listed("b", "Other.md", "2026-01-02T00:00:00Z")];
        let existing: HashMap<String, String> =
            [("b".to_string(), "Plan (2).md".to_string())].into();
        let paths = paths_of(&files, &existing, &HashSet::new());
        assert_eq!(paths, vec!["Other.md"]);
    }

    #[test]
    fn a_name_with_no_extension_takes_the_suffix_at_the_end() {
        let mut a = listed("a", "Notes", "2026-01-01T00:00:00Z");
        a.file.mime_type = SHEET_MIME.to_string();
        let mut b = listed("b", "Notes", "2026-01-02T00:00:00Z");
        b.file.mime_type = SHEET_MIME.to_string();
        let paths = paths_of(&[a, b], &HashMap::new(), &HashSet::new());
        assert_eq!(paths, vec!["Notes", "Notes (2)"]);
    }

    #[test]
    fn a_native_doc_is_a_markdown_path() {
        let mut doc = listed("d", "Plan", "2026-01-01T00:00:00Z");
        doc.file.mime_type = GOOGLE_DOC_MIME.to_string();
        let md = listed("m", "Plan.md", "2026-01-02T00:00:00Z");
        let paths = paths_of(&[doc, md], &HashMap::new(), &HashSet::new());
        assert_eq!(paths, vec!["Plan.md", "Plan (2).md"]);
    }

    #[test]
    fn a_new_file_skips_a_path_a_kept_file_holds() {
        let files = [listed("c", "Plan.md", "2026-01-03T00:00:00Z")];
        let occupied: HashSet<String> = ["plan.md".to_string(), "plan (2).md".to_string()].into();
        let paths = paths_of(&files, &HashMap::new(), &occupied);
        assert_eq!(paths, vec!["Plan (3).md"]);
    }

    #[test]
    fn unsafe_names_become_safe_segments_that_stay_in_the_root() {
        let files = [
            listed("a", "a/b.md", "2026-01-01T00:00:00Z"),
            listed("b", "..", "2026-01-01T00:00:00Z"),
            listed(
                "c",
                &format!("{}.md", "x".repeat(300)),
                "2026-01-01T00:00:00Z",
            ),
        ];
        let paths = paths_of(&files, &HashMap::new(), &HashSet::new());
        assert_eq!(paths[0], "a%2Fb.md");
        assert_eq!(paths[1], "%2E%2E");
        assert!(paths[2].len() <= 255 && !paths[2].contains('/'));
    }

    #[test]
    fn the_folder_text_forms() {
        assert_eq!(
            parse_folder("Notes").unwrap(),
            FolderSpec::Bare("Notes".to_string())
        );
        assert_eq!(
            parse_folder("/Work/Notes/").unwrap(),
            FolderSpec::Path(vec!["Work".to_string(), "Notes".to_string()])
        );
        assert_eq!(
            parse_folder("https://drive.google.com/drive/folders/1AbC_d-9?usp=sharing").unwrap(),
            FolderSpec::Id("1AbC_d-9".to_string())
        );
        assert_eq!(
            parse_folder("https://drive.google.com/drive/u/0/folders/1AbC").unwrap(),
            FolderSpec::Id("1AbC".to_string())
        );
        assert_eq!(
            parse_folder("https://drive.google.com/open?id=1AbC").unwrap(),
            FolderSpec::Id("1AbC".to_string())
        );
        for bad in [
            "",
            "   ",
            "/",
            "//",
            "..",
            "a//b",
            "a/../b",
            "https://example.com/drive/folders/abc",
            "https://drive.google.com/drive/my-drive",
        ] {
            assert!(parse_folder(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn google_files_get_their_own_kinds_and_are_never_text() {
        let mut f = listed("s", "Budget", "2026-01-01T00:00:00Z");
        for (mime, kind) in [
            (SHEET_MIME, "gsheet"),
            (SLIDES_MIME, "gslides"),
            (FORM_MIME, "gform"),
            (DRAWING_MIME, "gdrawing"),
            (SHORTCUT_MIME, "gshortcut"),
            ("application/vnd.google-apps.script", "binary"),
            (GOOGLE_DOC_MIME, "gdoc"),
        ] {
            f.file.mime_type = mime.to_string();
            assert_eq!(kind_and_name(&f.file).0, kind);
        }
        assert!(!tephra_cache::is_text_item_kind("gsheet"));
        assert!(tephra_cache::is_text_item_kind("gdoc"));
        let mut folder = f.file.clone();
        folder.mime_type = tephra_remote::drive::FOLDER_MIME.to_string();
        assert!(folder.is_folder());
    }
}
