use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        loop {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("twig-{label}-{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create fixture {}: {error}", path.display()),
            }
        }
    }

    fn root(&self) -> PathBuf {
        self.path.join("root")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        make_tree_owner_accessible(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn make_tree_owner_accessible(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    let _ = fs::set_permissions(path, permissions);
    if let Ok(children) = fs::read_dir(path) {
        for child in children.flatten() {
            make_tree_owner_accessible(&child.path());
        }
    }
}

#[cfg(not(unix))]
fn make_tree_owner_accessible(_path: &Path) {}

fn write_allocated_file(path: &Path, byte: u8) {
    fs::write(path, vec![byte; 16 * 1024])
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    assert!(
        allocated_size(path) > 0,
        "fixture file must allocate blocks"
    );
}

fn allocated_size(path: &Path) -> u64 {
    let metadata = fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()));
    allocated_size_from_metadata(&metadata)
}

#[cfg(unix)]
fn allocated_size_from_metadata(metadata: &fs::Metadata) -> u64 {
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_size_from_metadata(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

fn allocated_sum(paths: &[&Path]) -> u64 {
    paths.iter().map(|path| allocated_size(path)).sum()
}

fn run_twig(root: &Path, flags: &[&str]) -> Output {
    run_twig_paths(&[root], flags)
}

fn run_twig_paths(roots: &[&Path], flags: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_twig"));
    command
        .args(flags)
        .args([
            "-S",
            "-c",
            "-L",
            "--sort",
            "name",
            "--color",
            "never",
            "--hyperlink=never",
        ])
        .args(roots)
        // These are specifically live-traversal regressions. A nonexistent
        // read-only database prevents a running fsxd from changing the path.
        .env("FSX_INDEX_DB", roots[0].join(".missing-fsx-index.sqlite"));
    command
        .output()
        .unwrap_or_else(|error| panic!("run twig: {error}"))
}

#[derive(Debug, Eq, PartialEq)]
struct StatsRow {
    size: String,
    dirs: u64,
    files: u64,
}

fn stats_rows(output: &Output) -> HashMap<String, StatsRow> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            assert_eq!(
                fields.len(),
                4,
                "unexpected recursive-stat row {line:?} in {stdout:?}"
            );
            let dirs = fields[1]
                .parse()
                .unwrap_or_else(|error| panic!("parse dir count in {line:?}: {error}"));
            let files = fields[2]
                .parse()
                .unwrap_or_else(|error| panic!("parse file count in {line:?}: {error}"));
            (
                fields[3].to_string(),
                StatsRow {
                    size: fields[0].to_string(),
                    dirs,
                    files,
                },
            )
        })
        .collect()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "twig failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn expected_row(size: u64, dirs: u64, files: u64) -> StatsRow {
    StatsRow {
        size: fsx::format_size_compact(size),
        dirs,
        files,
    }
}

#[test]
fn recursive_child_and_root_totals_match_filesystem_metadata() {
    let fixture = Fixture::new("recursive-exact");
    let root = fixture.root();
    let child = root.join("child");
    let nested = child.join("nested");
    let empty = root.join("empty");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir(&empty).unwrap();
    let direct_file = child.join("direct.bin");
    let nested_file = nested.join("nested.bin");
    let root_file = root.join("root.bin");
    write_allocated_file(&direct_file, 0x11);
    write_allocated_file(&nested_file, 0x22);
    write_allocated_file(&root_file, 0x33);

    let child_size = allocated_sum(&[
        child.as_path(),
        nested.as_path(),
        direct_file.as_path(),
        nested_file.as_path(),
    ]);
    let empty_size = allocated_size(&empty);
    let root_size = allocated_sum(&[
        root.as_path(),
        child.as_path(),
        nested.as_path(),
        empty.as_path(),
        direct_file.as_path(),
        nested_file.as_path(),
        root_file.as_path(),
    ]);

    let children = run_twig(&root, &[]);
    assert_success(&children);
    let child_rows = stats_rows(&children);
    assert_eq!(child_rows["child"], expected_row(child_size, 2, 2));
    assert_eq!(child_rows["empty"], expected_row(empty_size, 1, 0));

    let root_output = run_twig(&root, &["-n"]);
    assert_success(&root_output);
    let root_rows = stats_rows(&root_output);
    assert_eq!(root_rows["root"], expected_row(root_size, 4, 3));
}

#[test]
fn hardlinks_dedupe_globally_and_independently_per_child() {
    let fixture = Fixture::new("recursive-hardlinks");
    let root = fixture.root();
    let same = root.join("same");
    let left = root.join("left");
    let right = root.join("right");
    fs::create_dir_all(&same).unwrap();
    fs::create_dir(&left).unwrap();
    fs::create_dir(&right).unwrap();

    let same_original = same.join("original.bin");
    let same_alias = same.join("alias.bin");
    write_allocated_file(&same_original, 0x44);
    fs::hard_link(&same_original, &same_alias).unwrap();

    let left_original = left.join("shared.bin");
    let right_alias = right.join("shared.bin");
    write_allocated_file(&left_original, 0x55);
    fs::hard_link(&left_original, &right_alias).unwrap();

    let same_data_size = allocated_size(&same_original);
    let cross_data_size = allocated_size(&left_original);
    let same_deduped_size = allocated_size(&same) + same_data_size;
    let same_counted_size = allocated_size(&same) + same_data_size.saturating_mul(2);
    let left_size = allocated_size(&left) + cross_data_size;
    let right_size = allocated_size(&right) + cross_data_size;
    let directory_size = allocated_sum(&[
        root.as_path(),
        same.as_path(),
        left.as_path(),
        right.as_path(),
    ]);
    let root_deduped_size = directory_size + same_data_size + cross_data_size;
    let root_counted_size =
        directory_size + same_data_size.saturating_mul(2) + cross_data_size.saturating_mul(2);

    let deduped = run_twig(&root, &[]);
    assert_success(&deduped);
    let deduped_rows = stats_rows(&deduped);
    assert_eq!(deduped_rows["same"], expected_row(same_deduped_size, 1, 2));
    assert_eq!(deduped_rows["left"], expected_row(left_size, 1, 1));
    assert_eq!(deduped_rows["right"], expected_row(right_size, 1, 1));

    let counted = run_twig(&root, &["-H"]);
    assert_success(&counted);
    let counted_rows = stats_rows(&counted);
    assert_eq!(counted_rows["same"], expected_row(same_counted_size, 1, 2));
    assert_eq!(counted_rows["left"], expected_row(left_size, 1, 1));
    assert_eq!(counted_rows["right"], expected_row(right_size, 1, 1));

    let deduped_root = run_twig(&root, &["-n"]);
    assert_success(&deduped_root);
    assert_eq!(
        stats_rows(&deduped_root)["root"],
        expected_row(root_deduped_size, 4, 4)
    );

    let counted_root = run_twig(&root, &["-n", "-H"]);
    assert_success(&counted_root);
    assert_eq!(
        stats_rows(&counted_root)["root"],
        expected_row(root_counted_size, 4, 4)
    );
}

#[cfg(unix)]
#[test]
fn dereferenced_symlink_root_uses_the_precomputed_root_totals() {
    let fixture = Fixture::new("recursive-root-symlink");
    let target = fixture.path.join("target");
    let child = target.join("child");
    let file = child.join("payload.bin");
    let link = fixture.path.join("root-link");
    fs::create_dir_all(&child).unwrap();
    write_allocated_file(&file, 0x66);
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let expected_size = allocated_sum(&[target.as_path(), child.as_path(), file.as_path()]);
    let output = run_twig(&link, &["-1", "-n"]);
    assert_success(&output);
    assert_eq!(
        stats_rows(&output)["root-link"],
        expected_row(expected_size, 2, 1)
    );
}

#[test]
fn hidden_descendants_contribute_to_visible_sizes_and_counts() {
    let fixture = Fixture::new("recursive-hidden");
    let root = fixture.root();
    let visible = root.join("visible");
    let hidden_dir = visible.join(".hidden-dir");
    fs::create_dir_all(&hidden_dir).unwrap();
    let hidden_file = visible.join(".hidden-file");
    let hidden_nested_file = hidden_dir.join("payload.bin");
    let hidden_root_file = root.join(".root-hidden.bin");
    write_allocated_file(&hidden_file, 0x66);
    write_allocated_file(&hidden_nested_file, 0x77);
    write_allocated_file(&hidden_root_file, 0x88);

    let visible_size = allocated_sum(&[
        visible.as_path(),
        hidden_dir.as_path(),
        hidden_file.as_path(),
        hidden_nested_file.as_path(),
    ]);
    let root_size = allocated_sum(&[
        root.as_path(),
        visible.as_path(),
        hidden_dir.as_path(),
        hidden_file.as_path(),
        hidden_nested_file.as_path(),
        hidden_root_file.as_path(),
    ]);

    let children = run_twig(&root, &[]);
    assert_success(&children);
    let stdout = String::from_utf8_lossy(&children.stdout);
    assert!(!stdout.contains(".hidden"), "hidden names leaked: {stdout}");
    assert_eq!(
        stats_rows(&children)["visible"],
        expected_row(visible_size, 2, 2)
    );

    let root_output = run_twig(&root, &["-n"]);
    assert_success(&root_output);
    assert_eq!(
        stats_rows(&root_output)["root"],
        expected_row(root_size, 3, 3)
    );
}

#[cfg(unix)]
#[test]
fn partial_live_scan_preserves_readable_child_and_fails_explicitly() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let fixture = Fixture::new("recursive-partial");
    let root = fixture.root();
    let readable = root.join("readable");
    let blocked = root.join("blocked");
    fs::create_dir_all(&readable).unwrap();
    fs::create_dir(&blocked).unwrap();
    let readable_file = readable.join("visible.bin");
    let blocked_file = blocked.join("secret.bin");
    write_allocated_file(&readable_file, 0x99);
    write_allocated_file(&blocked_file, 0xaa);
    let readable_size = allocated_size(&readable) + allocated_size(&readable_file);

    let mut blocked_permissions = fs::symlink_metadata(&blocked).unwrap().permissions();
    blocked_permissions.set_mode(0o0);
    fs::set_permissions(&blocked, blocked_permissions).unwrap();
    if fs::read_dir(&blocked).is_ok() {
        make_tree_owner_accessible(&blocked);
        return;
    }

    let output = run_twig(&root, &[]);
    make_tree_owner_accessible(&blocked);

    assert!(
        !output.status.success(),
        "partial scan unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stats_rows(&output)["readable"],
        expected_row(readable_size, 1, 1)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr, "twig: recursive scan incomplete: 1 unreadable entry\n",
        "partial scan must report its count exactly once"
    );
}

#[cfg(unix)]
#[test]
fn partial_scan_combines_error_counts_across_operands() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let fixture = Fixture::new("recursive-error-counts");
    let first = fixture.root();
    let second = fixture.path.join("second");
    let blocked = [first.join("a"), first.join("b"), second.join("c")];
    for path in &blocked {
        fs::create_dir_all(path).unwrap();
        // Unvisited descendants must not be guessed or added to the failure count.
        write_allocated_file(&path.join("unvisited.bin"), 0xaa);
        fs::set_permissions(path, fs::Permissions::from_mode(0o0)).unwrap();
        if fs::read_dir(path).is_ok() {
            return;
        }
    }

    for flags in [vec![], vec!["-l"]] {
        let output = run_twig(&first, &flags);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "twig: recursive scan incomplete: 2 unreadable entries\n"
        );

        let output = run_twig_paths(&[&first, &second], &flags);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "twig: recursive scan incomplete: 3 unreadable entries\n"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("root"), "first operand omitted: {stdout}");
        assert!(
            stdout.contains("second"),
            "second operand omitted: {stdout}"
        );
    }

    for path in &blocked {
        make_tree_owner_accessible(path);
    }
    let output = run_twig_paths(&[&first, &second], &[]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
}
