//! Task-scoped review state, stored in worktree Git metadata, never project files.
use crate::tasks::Task;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

const FILE_LIMIT: usize = 128 * 1024;
const SNAPSHOT_LIMIT: usize = 4 * 1024 * 1024;
static STORE: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
pub struct FileDiff {
    pub path: String,
    pub patch: String,
    pub fingerprint: u64,
    pub truncated: bool,
    pub unstable: bool,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub files: Vec<FileDiff>,
    pub fingerprint: u64,
    pub partial: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewState {
    pub reviewed: BTreeMap<String, u64>,
    pub comments: Vec<Comment>,
    pub checks: Vec<Verification>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub text: String,
    pub patch: String,
    pub revision: u64,
}

impl Comment {
    pub fn prompt(&self, task: &Task) -> String {
        let context = self
            .patch
            .lines()
            .map(|line| format!("    {line}\n"))
            .collect::<String>();
        format!("Review feedback for task {}\n{}:{}-{}\n\n{}\n\nReviewed patch context:\n{}\nPlease address this feedback and report the verification performed.\n", task.name, self.path, self.line_start, self.line_end, self.text, context)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Verification {
    pub command: String,
    pub exit: Option<i32>,
    pub output: String,
    pub revision: u64,
    pub finished_at: u64,
}

#[derive(Clone, Debug)]
pub struct Review {
    pub snapshot: Snapshot,
    pub state: ReviewState,
}

/// Capture a bounded Git response. A truncated patch is explicitly marked and
/// cannot be marked fully reviewed. No external diff/textconv driver is run.
fn git(dir: &Path, args: &[&str], limit: usize, diff_exit: bool) -> Result<(String, bool), String> {
    let mut child = mtty_platform::background_command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .unwrap()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes);
    let truncated = bytes.len() > limit;
    if truncated || read.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    read.map_err(|e| e.to_string())?;
    if !truncated && !status.success() && !(diff_exit && status.code() == Some(1)) {
        return Err(format!(
            "git {} failed ({status})",
            args.first().unwrap_or(&"")
        ));
    }
    bytes.truncate(limit);
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

fn content_hash(path: &Path) -> Result<u64, String> {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            false.hash(&mut hash);
            return Ok(hash.finish());
        }
        Err(error) => return Err(error.to_string()),
    };
    true.hash(&mut hash);
    if metadata.file_type().is_symlink() {
        std::fs::read_link(path)
            .map_err(|e| e.to_string())?
            .hash(&mut hash);
    } else if metadata.is_file() {
        let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut buffer = [0; 8192];
        loop {
            let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.write(&buffer[..n]);
        }
    }
    Ok(hash.finish())
}

pub fn snapshot(task: &Task) -> Result<Snapshot, String> {
    let (base, _) = git(&task.path, &["merge-base", "HEAD", &task.base], 1024, false)?;
    let base = base.trim();
    let (tracked, tracked_cut) = git(
        &task.path,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "--no-renames",
            "-z",
            base,
            "--",
        ],
        1024 * 1024,
        false,
    )?;
    let (others, others_cut) = git(
        &task.path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        1024 * 1024,
        false,
    )?;
    let untracked: BTreeSet<_> = others.split('\0').filter(|s| !s.is_empty()).collect();
    let paths: BTreeSet<_> = tracked
        .split('\0')
        .chain(others.split('\0'))
        .filter(|s| !s.is_empty())
        .collect();
    let mut partial = tracked_cut || others_cut || paths.len() > 200;
    let mut remaining = SNAPSHOT_LIMIT;
    let mut files = Vec::new();
    let mut signature = std::collections::hash_map::DefaultHasher::new();
    base.hash(&mut signature);
    for path in paths.into_iter().take(200) {
        if remaining == 0 {
            partial = true;
            break;
        }
        let before = content_hash(&task.path.join(path))?;
        let (patch, truncated) = if untracked.contains(path) {
            git(
                &task.path,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--no-index",
                    "--",
                    "/dev/null",
                    path,
                ],
                remaining.min(FILE_LIMIT),
                true,
            )?
        } else {
            git(
                &task.path,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--no-renames",
                    base,
                    "--",
                    path,
                ],
                remaining.min(FILE_LIMIT),
                false,
            )?
        };
        remaining = remaining.saturating_sub(patch.len());
        let after = content_hash(&task.path.join(path))?;
        let unstable = before != after;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hash);
        patch.hash(&mut hash);
        before.hash(&mut hash);
        let fingerprint = hash.finish();
        path.hash(&mut signature);
        fingerprint.hash(&mut signature);
        partial |= truncated || unstable;
        files.push(FileDiff {
            path: path.to_owned(),
            patch,
            fingerprint,
            truncated,
            unstable,
        });
    }
    Ok(Snapshot {
        files,
        fingerprint: signature.finish(),
        partial,
    })
}

fn state_path(task: &Task) -> Result<PathBuf, String> {
    let (path, _) = git(
        &task.path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "mtty-review.json",
        ],
        8192,
        false,
    )?;
    Ok(PathBuf::from(path.trim()))
}

fn state(task: &Task) -> Result<ReviewState, String> {
    let path = state_path(task)?;
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ReviewState::default()),
        Err(error) => Err(error.to_string()),
    }
}

fn store(task: &Task, state: &ReviewState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?;
    let path = state_path(task)?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    let result = std::fs::rename(&temp, path).map_err(|e| e.to_string());
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

pub fn load(task: &Task) -> Result<Review, String> {
    let snapshot = snapshot(task)?;
    let _lock = STORE.lock().unwrap_or_else(|e| e.into_inner());
    Ok(Review {
        snapshot,
        state: state(task)?,
    })
}

pub fn mark_reviewed(
    task: &Task,
    path: &str,
    fingerprint: u64,
    reviewed: bool,
) -> Result<Review, String> {
    let fresh = snapshot(task)?;
    let file = fresh
        .files
        .iter()
        .find(|file| file.path == path)
        .ok_or("file no longer appears in the task diff")?;
    if reviewed && (file.fingerprint != fingerprint || file.truncated || file.unstable) {
        return Err(
            "changes moved or the diff is partial; refresh and review the current content".into(),
        );
    }
    let _lock = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = state(task)?;
    if reviewed {
        state.reviewed.insert(path.to_owned(), fingerprint);
    } else {
        state.reviewed.remove(path);
    }
    store(task, &state)?;
    Ok(Review {
        snapshot: fresh,
        state,
    })
}

pub fn record_comment(task: &Task, mut comment: Comment) -> Result<Review, String> {
    comment.text = comment.text.chars().take(8192).collect();
    comment.patch = comment.patch.chars().take(8192).collect();
    {
        let _lock = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = state(task)?;
        state.comments.push(comment);
        if state.comments.len() > 10 {
            state.comments.remove(0);
        }
        store(task, &state)?;
    }
    load(task)
}

pub fn record_check(task: &Task, mut check: Verification) -> Result<Review, String> {
    check.output = check.output.chars().take(64 * 1024).collect();
    {
        let _lock = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = state(task)?;
        state.checks.push(check);
        if state.checks.len() > 5 {
            state.checks.remove(0);
        }
        store(task, &state)?;
    }
    load(task)
}

/// The new-file line addressed by a unified-diff row, or None for removed
/// lines and headers. A deletion comment can instead use an explicit range.
pub fn line_numbers(patch: &str) -> Vec<Option<usize>> {
    let mut next = 0;
    patch
        .lines()
        .map(|line| {
            if let Some(header) = line.strip_prefix("@@ ") {
                next = header
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.strip_prefix('+'))
                    .and_then(|s| s.split(',').next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                None
            } else if next > 0 && (line.starts_with(' ') || line.starts_with('+')) {
                let current = next;
                next += 1;
                Some(current)
            } else {
                None
            }
        })
        .collect()
}

/// Select the relevant rows rather than always sending the beginning of a file.
pub fn comment_context(patch: &str, first: usize, last: usize) -> String {
    let lines: Vec<_> = patch.lines().collect();
    let numbers = line_numbers(patch);
    let start = numbers
        .iter()
        .position(|line| line.is_some_and(|line| line >= first))
        .unwrap_or(0);
    let end = numbers
        .iter()
        .rposition(|line| line.is_some_and(|line| line <= last))
        .unwrap_or(start)
        .max(start);
    let from = start.saturating_sub(6);
    let to = (end + 7).min(lines.len()).min(from + 200);
    lines[from..to].join("\n").chars().take(8192).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn command(dir: &Path, args: &[&str]) {
        git(dir, args, 1024 * 1024, false).unwrap();
    }
    #[test]
    fn reviews_include_all_change_sources_and_invalidate_after_edits() {
        let root = std::env::temp_dir().join(format!(
            "mtty-review-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        command(&root, &["init", "-q", "-b", "main"]);
        command(&root, &["config", "user.name", "Review Test"]);
        command(&root, &["config", "user.email", "review@example.com"]);
        command(&root, &["config", "commit.gpgsign", "false"]);
        command(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
        std::fs::write(root.join("deleted.txt"), "remove me\n").unwrap();
        command(&root, &["add", "."]);
        command(&root, &["commit", "-qm", "base"]);
        let task = crate::tasks::create(&root, "review").unwrap();
        std::fs::write(task.path.join("committed.txt"), "committed\n").unwrap();
        command(&task.path, &["add", "committed.txt"]);
        command(&task.path, &["commit", "-qm", "committed"]);
        std::fs::write(task.path.join("tracked.txt"), "staged\n").unwrap();
        command(&task.path, &["add", "tracked.txt"]);
        std::fs::write(task.path.join("tracked.txt"), "staged\nunstaged\n").unwrap();
        std::fs::write(task.path.join("new file.txt"), "untracked content\n").unwrap();
        std::fs::write(task.path.join("binary.bin"), [0, 1, 2, 0]).unwrap();
        std::fs::remove_file(task.path.join("deleted.txt")).unwrap();
        let review = load(&task).unwrap();
        assert_eq!(review.snapshot.files.len(), 5);
        assert!(review
            .snapshot
            .files
            .iter()
            .any(|file| file.path == "new file.txt" && file.patch.contains("+untracked content")));
        let tracked = review
            .snapshot
            .files
            .iter()
            .find(|file| file.path == "tracked.txt")
            .unwrap();
        assert!(tracked.patch.contains("+unstaged"));
        let marked = mark_reviewed(&task, &tracked.path, tracked.fingerprint, true).unwrap();
        assert_eq!(
            marked.state.reviewed.get("tracked.txt"),
            Some(&tracked.fingerprint)
        );
        std::fs::write(task.path.join("tracked.txt"), "new revision\n").unwrap();
        assert!(mark_reviewed(&task, &tracked.path, tracked.fingerprint, true).is_err());
        let current = load(&task).unwrap();
        let now = current
            .snapshot
            .files
            .iter()
            .find(|file| file.path == "tracked.txt")
            .unwrap();
        assert_ne!(
            current.state.reviewed.get("tracked.txt"),
            Some(&now.fingerprint)
        );
        let check = Verification {
            command: "check".into(),
            exit: Some(1),
            output: "failed".into(),
            revision: current.snapshot.fingerprint,
            finished_at: 1,
        };
        let checked = record_check(&task, check).unwrap();
        assert_eq!(checked.state.checks[0].exit, Some(1));
        assert!(!git(&task.path, &["status", "--porcelain"], 8192, false)
            .unwrap()
            .0
            .contains("mtty-review.json"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn diff_rows_map_context_and_added_lines_without_addressing_headers() {
        assert_eq!(
            line_numbers("+++ b/a\n@@ -3,2 +4,3 @@\n context\n-old\n+new\n+extra\n"),
            vec![None, None, Some(4), None, Some(5), Some(6)]
        );
    }

    #[test]
    fn feedback_includes_the_selected_hunk_beyond_the_first_chunk() {
        let patch = format!(
            "@@ -1,0 +1,400 @@\n{}",
            (1..=400)
                .map(|line| format!("+row {line}\n"))
                .collect::<String>()
        );
        let context = comment_context(&patch, 350, 352);
        assert!(context.contains("+row 350\n+row 351\n+row 352"));
        assert!(!context.contains("+row 1\n"));
    }
}
