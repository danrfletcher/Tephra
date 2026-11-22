//! Where a remote file lives on disk, and the rules that keep it there.
//!
//! A remote name is not a safe file name: a Drive name may hold `/`, be
//! `..` or `.git`, be empty, be 300 bytes long; a Git path may be absolute
//! if a tree says so. [`safe_segments`] maps a remote path to the names of
//! the directories and file below the cache root, and [`resolve_under`]
//! turns them into a path after checking, one component at a time and
//! without following anything, that no existing component is a symlink.
//!
//! The mapping is
//!
//! - deterministic: the same remote path always gives the same names;
//! - injective: two different remote paths never give the same names. A
//!   `%` is the only escape character, and a raw `%` is itself escaped, so
//!   no escaped text can be read two ways;
//! - rooted: every name is one plain component (no `/`, never `.` or
//!   `..`), so the joined path cannot leave the root.
//!
//! The rules, per `/`-separated segment of the remote path:
//!
//! - an empty segment (from `a//b`, a leading `/` or a trailing `/`) is `%`;
//! - `.` is `%2E` and `..` is `%2E%2E`;
//! - `%`, `\`, `/` and every control character (NUL included) are written
//!   as `%XX` per byte;
//! - a name that is `.git` in any letter case starts `%2E` instead, so
//!   nothing a remote lists can be, or replace, a git directory;
//! - a name that grows past 255 bytes is cut on a character boundary and
//!   ends in `%~` and 24 hex digits of the SHA-1 of the whole original
//!   name, so two long names with one start stay different;
//! - names of one source that differ only in letter case are kept apart on
//!   a case-insensitive disk by [`case_distinct`], which the fetch applies
//!   when the listing has such a pair.
//!
//! No mapped name starts with `%t`, so the temporary files of a fetch
//! ([`temp_name`]) never collide with a mapped name, and a sweep of them
//! ([`is_temp_name`]) never touches a fetched file.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use tephra_remote::github::git_blob_sha;

/// The longest file name the common file systems take.
pub const MAX_NAME_BYTES: usize = 255;

/// What starts the name of a fetch's temporary file.
const TEMP_PREFIX: &str = "%tmp-";

/// Why a path could not be used.
#[derive(Debug)]
pub enum SafePathError {
    /// A name that is not one plain component (a bug in the caller: the
    /// mapper never produces one).
    Unsafe(String),
    /// A component that exists is a symlink, which is never followed or
    /// written through.
    Symlink(PathBuf),
    /// A directory is expected and something else is there.
    NotADirectory(PathBuf),
    /// A file is expected and a directory is there.
    IsDirectory(PathBuf),
    Io(io::Error),
}

impl fmt::Display for SafePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SafePathError::Unsafe(name) => write!(f, "unsafe cache name {name:?}"),
            SafePathError::Symlink(p) => {
                write!(
                    f,
                    "{} is a symlink; the cache never follows one",
                    p.display()
                )
            }
            SafePathError::NotADirectory(p) => {
                write!(f, "{} is in the way and is not a directory", p.display())
            }
            SafePathError::IsDirectory(p) => {
                write!(f, "{} is a directory, not a file", p.display())
            }
            SafePathError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SafePathError {}

impl From<io::Error> for SafePathError {
    fn from(e: io::Error) -> Self {
        SafePathError::Io(e)
    }
}

/// `hash_len` hex digits of a SHA-1 of `text` (git's blob hash of it: a
/// fixed, well-spread digest; nothing here depends on it being secret).
fn digest(text: &str, hash_len: usize) -> String {
    git_blob_sha(text.as_bytes())[..hash_len].to_string()
}

fn escape_char(c: char, out: &mut String) {
    let mut buf = [0u8; 4];
    for b in c.encode_utf8(&mut buf).bytes() {
        out.push_str(&format!("%{b:02X}"));
    }
}

/// `name` with `marker` and `hash` at its end, cut so the whole stays within
/// [`MAX_NAME_BYTES`]. The cut is on a character boundary and never inside
/// a `%XX` escape.
fn with_marker(name: &str, marker: &str, hash: &str) -> String {
    let tail = format!("{marker}{hash}");
    let room = MAX_NAME_BYTES - tail.len();
    let mut cut = name.len().min(room);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    // Not inside an escape: a `%` in the last two bytes starts one that the
    // cut may have split.
    let head = &name[..cut];
    let bytes = head.as_bytes();
    if let Some(at) = bytes.iter().rposition(|b| *b == b'%') {
        if bytes.len() - at < 3 {
            cut = at;
        }
    }
    format!("{}{tail}", &name[..cut])
}

/// One remote name (no `/` in it) as one safe file name. See the module
/// documentation for the rules.
pub fn safe_segment(name: &str) -> String {
    match name {
        "" => return "%".to_string(),
        "." => return "%2E".to_string(),
        ".." => return "%2E%2E".to_string(),
        _ => {}
    }
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '%' | '\\' | '/' => escape_char(c, &mut out),
            c if c.is_control() => escape_char(c, &mut out),
            c => out.push(c),
        }
    }
    if out.eq_ignore_ascii_case(".git") {
        out.replace_range(0..1, "%2E");
    }
    if out.len() > MAX_NAME_BYTES {
        out = with_marker(&out, "%~", &digest(name, 24));
    }
    out
}

/// A remote path as the names of the directories and the file below the
/// cache root, one per `/`-separated segment.
pub fn safe_segments(remote_path: &str) -> Vec<String> {
    remote_path.split('/').map(safe_segment).collect()
}

/// The names of a path whose file has a sibling that differs from it only
/// by letter case: the file's name ends in `%^` and 12 hex digits of the
/// SHA-1 of the whole remote path (case included), so the two stay two
/// files on a case-insensitive disk. The directories are unchanged.
pub fn case_distinct(remote_path: &str) -> Vec<String> {
    let mut segments = safe_segments(remote_path);
    if let Some(last) = segments.last_mut() {
        *last = with_marker(last, "%^", &digest(remote_path, 12));
    }
    segments
}

/// The name of one temporary file of a fetch, in the directory of the file
/// it becomes (so the rename never crosses a file system).
pub fn temp_name(serial: u64) -> String {
    format!("{TEMP_PREFIX}{}-{serial}", std::process::id())
}

/// Whether `name` is the name of a fetch's temporary file.
pub fn is_temp_name(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX)
}

/// Whether `name` is one plain component of a path: not empty, not `.` or
/// `..`, holding no `/`, `\` or control character.
fn is_plain(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

/// What `lstat` finds at `path`: nothing, or the file type.
fn lstat(path: &Path) -> io::Result<Option<std::fs::FileType>> {
    match std::fs::symlink_metadata(path) {
        Ok(md) => Ok(Some(md.file_type())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `base` joined with `segments` (from [`safe_segments`] or
/// [`case_distinct`], with any directories above them), after checking
/// every component below `base` without following anything.
///
/// - A component that exists as a symlink is [`SafePathError::Symlink`], the
///   last one too: a file is never written through a link.
/// - A directory that is missing is created when `create` is set (one level
///   at a time, each checked again after it is made) and otherwise ends the
///   check: nothing is there to be a link.
/// - The last component may be missing; it is a file, so a directory there
///   is [`SafePathError::IsDirectory`].
///
/// `base` itself is trusted (it is the daemon's data directory, which may
/// itself be reached through a link, as `/var` is on macOS).
pub fn resolve_under(
    base: &Path,
    segments: &[String],
    create: bool,
) -> Result<PathBuf, SafePathError> {
    let mut path = base.to_path_buf();
    let mut checking = true;
    for (i, name) in segments.iter().enumerate() {
        if !is_plain(name) {
            return Err(SafePathError::Unsafe(name.clone()));
        }
        path.push(name);
        let last = i + 1 == segments.len();
        if !checking {
            continue;
        }
        match lstat(&path)? {
            Some(t) if t.is_symlink() => return Err(SafePathError::Symlink(path)),
            Some(t) if last && t.is_dir() => return Err(SafePathError::IsDirectory(path)),
            Some(t) if !last && !t.is_dir() => return Err(SafePathError::NotADirectory(path)),
            Some(_) => {}
            None if last => {}
            None if create => {
                match std::fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
                // Made by us, or by someone between the check and the
                // `create_dir`: look again.
                match lstat(&path)? {
                    Some(t) if t.is_dir() => {}
                    Some(t) if t.is_symlink() => return Err(SafePathError::Symlink(path)),
                    _ => return Err(SafePathError::NotADirectory(path)),
                }
            }
            None => checking = false,
        }
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The path stays under `base`: nothing but plain components was added.
    fn stays_under(base: &Path, joined: &Path) -> bool {
        let Ok(rest) = joined.strip_prefix(base) else {
            return false;
        };
        rest.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    }

    fn seg(name: &str) -> String {
        safe_segment(name)
    }

    #[test]
    fn plain_names_are_unchanged() {
        for name in [
            "README.md",
            "a b.txt",
            "naïve.md",
            "日本語.md",
            "x-y_z.rs",
            ".hidden",
        ] {
            assert_eq!(seg(name), name);
        }
        assert_eq!(
            safe_segments("docs/Guide.md"),
            vec!["docs".to_string(), "Guide.md".to_string()]
        );
    }

    /// E10, rule by rule: what each hostile name becomes.
    #[test]
    fn every_rule_maps_to_the_documented_name() {
        let table: &[(&str, &str)] = &[
            // dot segments
            (".", "%2E"),
            ("..", "%2E%2E"),
            // empty
            ("", "%"),
            // the escape character itself
            ("100%", "100%25"),
            ("%2E", "%252E"),
            // separators that survive a segment split
            ("a\\b", "a%5Cb"),
            ("a/b", "a%2Fb"),
            // NUL and control characters
            ("a\0b", "a%00b"),
            ("tab\there", "tab%09here"),
            ("nl\n", "nl%0A"),
            ("del\u{7f}", "del%7F"),
            ("c1\u{85}", "c1%C2%85"),
            // `.git` in any case
            (".git", "%2Egit"),
            (".GIT", "%2EGIT"),
            (".Git", "%2EGit"),
            // names that only start like it are plain
            (".gitignore", ".gitignore"),
            (".git ", ".git "),
            ("..git", "..git"),
        ];
        for (raw, want) in table {
            assert_eq!(&seg(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn a_path_is_mapped_per_segment_and_an_absolute_path_is_rooted() {
        assert_eq!(
            safe_segments("/etc/passwd"),
            vec!["%".to_string(), "etc".to_string(), "passwd".to_string()]
        );
        assert_eq!(
            safe_segments("../../x"),
            vec!["%2E%2E".to_string(), "%2E%2E".to_string(), "x".to_string()]
        );
        assert_eq!(
            safe_segments("a//b/"),
            vec![
                "a".to_string(),
                "%".to_string(),
                "b".to_string(),
                "%".to_string()
            ]
        );
        assert_eq!(
            safe_segments("dir/.git/config"),
            vec![
                "dir".to_string(),
                "%2Egit".to_string(),
                "config".to_string()
            ]
        );
        assert_eq!(safe_segments(""), vec!["%".to_string()]);
    }

    #[test]
    fn a_long_name_is_cut_on_a_character_boundary_and_keeps_its_identity() {
        // 300 ASCII bytes.
        let long = "n".repeat(300);
        let mapped = seg(&long);
        assert!(mapped.len() <= MAX_NAME_BYTES, "{}", mapped.len());
        assert!(mapped.contains("%~"));
        // Two long names with one start differ.
        let other = format!("{}{}", "n".repeat(299), "m");
        assert_ne!(seg(&other), mapped);

        // Multi-byte characters: the cut never splits one, at any offset.
        for pad in 0..4 {
            let name = format!("{}{}", "x".repeat(pad), "é".repeat(200));
            let mapped = seg(&name);
            assert!(mapped.len() <= MAX_NAME_BYTES);
            assert!(std::str::from_utf8(mapped.as_bytes()).is_ok());
        }
        // Escapes are not split either: no `%` followed by fewer than two
        // characters before the marker.
        let name = format!("{}\u{1}\u{1}\u{1}", "y".repeat(226));
        let mapped = seg(&name);
        assert!(mapped.len() <= MAX_NAME_BYTES);
        let before_marker = mapped.split("%~").next().unwrap();
        assert!(
            !before_marker.ends_with('%')
                && !before_marker
                    .rsplit_once('%')
                    .is_some_and(|(_, t)| t.len() < 2),
            "{mapped}"
        );
        // A name at the limit is not cut.
        let exact = "z".repeat(MAX_NAME_BYTES);
        assert_eq!(seg(&exact), exact);
    }

    #[test]
    fn names_differing_in_case_map_to_different_names() {
        assert_ne!(seg("README.md"), seg("readme.md"));
        assert_ne!(seg(".git"), seg(".GIT"));
    }

    #[test]
    fn case_distinct_keeps_case_siblings_apart_and_stays_short() {
        let a = case_distinct("docs/README.md");
        let b = case_distinct("docs/readme.md");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0], "docs");
        assert_ne!(a[1].to_lowercase(), b[1].to_lowercase());
        assert!(a[1].starts_with("README.md%^"));
        // A name at the limit stays within it with the suffix.
        let long = format!("d/{}", "q".repeat(255));
        assert!(case_distinct(&long)[1].len() <= MAX_NAME_BYTES);
        // Deterministic.
        assert_eq!(case_distinct("docs/README.md"), a);
    }

    /// A small deterministic generator, so the property test needs no
    /// crate and gives the same names on every run.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
            &of[(self.next() as usize) % of.len()]
        }
    }

    fn random_path(rng: &mut Lcg) -> String {
        const PIECES: &[&str] = &[
            "a", "B", "é", "日", "..", ".", "", "/", "\\", "%", "%2E", ".git", ".GIT", "\0", "\n",
            "\u{7f}", " ", "tmp", "%tmp-1-1", "%~", "%^",
        ];
        let n = 1 + rng.next() % 12;
        (0..n)
            .map(|_| *rng.pick(PIECES))
            .collect::<Vec<_>>()
            .concat()
    }

    /// Property: whatever the remote path, the mapped names are plain
    /// components, the joined path is under the root, nothing is longer than
    /// 255 bytes, and a second path with another text never gets the same
    /// names.
    #[test]
    fn no_mapped_path_escapes_the_root_and_the_mapping_is_injective() {
        let base = Path::new("/data/remote/src/clone");
        let mut rng = Lcg(0xC0FFEE);
        let mut seen: HashMap<Vec<String>, String> = HashMap::new();
        for _ in 0..20_000 {
            let raw = random_path(&mut rng);
            let segments = safe_segments(&raw);
            assert!(!segments.is_empty());
            for s in &segments {
                assert!(is_plain(s) || s == "%", "{raw:?} -> {s:?}");
                assert!(s.len() <= MAX_NAME_BYTES, "{raw:?}");
                assert!(!is_temp_name(s), "{raw:?} -> {s:?}");
                assert!(!s.eq_ignore_ascii_case(".git"), "{raw:?}");
            }
            let joined = segments.iter().fold(base.to_path_buf(), |p, s| p.join(s));
            assert!(stays_under(base, &joined), "{raw:?} -> {joined:?}");
            // Deterministic.
            assert_eq!(segments, safe_segments(&raw));
            // Injective.
            if let Some(prev) = seen.insert(segments.clone(), raw.clone()) {
                assert_eq!(prev, raw, "{prev:?} and {raw:?} share {segments:?}");
            }
        }
    }

    #[test]
    fn temp_names_are_never_a_mapped_name() {
        let t = temp_name(7);
        assert!(is_temp_name(&t));
        assert!(is_plain(&t));
        assert!(!is_temp_name(&seg("%tmp-1-1")));
        assert!(!is_temp_name("README.md"));
    }

    #[test]
    fn resolve_under_creates_directories_and_returns_the_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let segments = safe_segments("a/b/c.txt");
        let path = resolve_under(dir.path(), &segments, true).unwrap();
        assert_eq!(path, dir.path().join("a").join("b").join("c.txt"));
        assert!(dir.path().join("a/b").is_dir());
        assert!(!path.exists(), "the file itself is not created");
    }

    #[test]
    fn resolve_under_without_create_makes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = resolve_under(dir.path(), &safe_segments("a/b/c.txt"), false).unwrap();
        assert_eq!(path, dir.path().join("a/b/c.txt"));
        assert!(!dir.path().join("a").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_cache_is_never_followed_or_written_through() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("victim"), b"keep").unwrap();

        // A directory component that is a link.
        symlink(outside.path(), dir.path().join("linked")).unwrap();
        for create in [true, false] {
            let err = resolve_under(dir.path(), &safe_segments("linked/victim"), create);
            assert!(matches!(err, Err(SafePathError::Symlink(_))), "{err:?}");
        }
        // The file itself is a link.
        symlink(outside.path().join("victim"), dir.path().join("file")).unwrap();
        let err = resolve_under(dir.path(), &safe_segments("file"), true);
        assert!(matches!(err, Err(SafePathError::Symlink(_))), "{err:?}");
        // A dangling link counts too.
        symlink(dir.path().join("nowhere"), dir.path().join("dangling")).unwrap();
        let err = resolve_under(dir.path(), &safe_segments("dangling"), true);
        assert!(matches!(err, Err(SafePathError::Symlink(_))), "{err:?}");
        assert_eq!(
            std::fs::read(outside.path().join("victim")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn resolve_under_refuses_a_file_where_a_directory_belongs_and_the_reverse() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let err = resolve_under(dir.path(), &safe_segments("f/x"), true);
        assert!(
            matches!(err, Err(SafePathError::NotADirectory(_))),
            "{err:?}"
        );
        let err = resolve_under(dir.path(), &safe_segments("d"), true);
        assert!(matches!(err, Err(SafePathError::IsDirectory(_))), "{err:?}");
    }

    #[test]
    fn resolve_under_refuses_a_name_that_is_not_one_component() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["..", ".", "", "a/b", "a\\b", "a\0b"] {
            let err = resolve_under(dir.path(), &[bad.to_string()], true);
            assert!(matches!(err, Err(SafePathError::Unsafe(_))), "{bad:?}");
        }
    }
}
