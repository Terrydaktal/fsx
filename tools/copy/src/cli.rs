//! Command-line parsing and help text.

use crate::domain::MergeCollisionPolicy;
use crate::plan::parse_merge_collision_policy;
use std::env;
use std::ffi::OsString;

#[derive(Default)]
pub(crate) struct CliArgs {
    pub(crate) source: String,
    pub(crate) destination: String,
    pub(crate) extra: Vec<String>,
    pub(crate) move_mode: bool,
    pub(crate) sudo: bool,
    pub(crate) overwrite: bool,
    pub(crate) contents_only: bool,
    pub(crate) create_destination_parents: bool,
    pub(crate) backup: bool,
    pub(crate) sync_mode: bool,
    pub(crate) showall: bool,
    pub(crate) tree_depth: Option<usize>,
    pub(crate) tree_trunc: usize,
    pub(crate) preview_only: bool,
    pub(crate) preview_lite: bool,
    pub(crate) replace_dest_symlink: bool,
    pub(crate) merge_collision_policy: MergeCollisionPolicy,
}
pub(crate) fn usage() {
    eprintln!(
        "usage: copy [-h] [-m] [-s] [-o] [-c] [--create-destination-parents] [-b] [--sync] [--replace-dest-symlink] [-v|--verbose|--showall] [-L depth] [-T trunc] [--preview] [--preview-lite] source... destination"
    );
}

fn set_merge_collision_policy(args: &mut CliArgs, policy: MergeCollisionPolicy) -> Result<(), i32> {
    if args.merge_collision_policy != MergeCollisionPolicy::default()
        && args.merge_collision_policy != policy
    {
        usage();
        eprintln!("copy: error: collision policy options are mutually exclusive");
        return Err(1);
    }
    args.merge_collision_policy = policy;
    Ok(())
}

#[allow(clippy::print_literal)]
pub(crate) fn print_help() {
    println!(
        "usage: copy [-h] [-m] [-s] [-o] [-c] [--create-destination-parents] [-b] [--sync] [--replace-dest-symlink] [-v|--verbose|--showall] [-L depth] [-T trunc] [--preview] [--preview-lite] source... destination"
    );
    println!();
    println!("Standalone copy/move with preview/progress.");
    println!("Supports local paths and one-sided remote rsync endpoints like user@host:/path or host:/path.");
    println!("Remote mode reads ~/.ssh/config for Host/User matching and uses rsync over SSH.");
    println!("Local mode preflight checks destination free space against planned transfer bytes (no sudo required).");
    println!();
    println!("positional arguments:");
    println!("  source                Source path (file or directory). Multiple files are supported when they all map to the same destination directory.");
    println!(
        "  destination           Destination path (directory, or file path when source is a file)"
    );
    println!();
    println!("options:");
    println!("  -h, --help            show this help message and exit");
    println!("  -m, --move            Move mode: transfer then remove source data (equivalent to move behavior).");
    println!("  -s, --sudo            Run privileged transfer commands through pkexec");
    println!(
        "  -o, --overwrite       Replace the destination target itself instead of merging it."
    );
    println!("                        This does not control file-vs-file collisions inside a merged folder.");
    println!("  -c, --contents-only   Transfer source directory children into destination (like source/*; do not nest source basename).");
    println!("                        In --move mode, source directories are removed if they become empty.");
    println!("  --create-destination-parents");
    println!("                        Create missing destination parent directories before planning the transfer.");
    println!(
        "                        The final destination target still follows the normal path rules."
    );
    println!("  -b, --backup          Create a timestamped backup when destination data will be merged or overwritten.");
    println!("  --sync              Native exact-tree sync with destination deletions (rsync-style semantics).");
    println!("                        Local transfers use the Rust backend; remote or --sudo transfers use rsync.");
    println!("                        Merge/sync semantics; not target replacement semantics like --overwrite.");
    println!("  -v, --verbose, --showall");
    println!("                        Show full preview tree (new, modified, identical, uncollided, deleted).");
    println!("  --collision policy   Collision policy for file-vs-file conflicts inside local Rust merges.");
    println!("                        Syntax: winner:conditions");
    println!("                        winner: source | dest");
    println!("                        conditions: always | newer | larger | size-differs");
    println!("                        Use ',' for OR and '+' for AND.");
    println!("                        Default: source:size-differs");
    println!("                        Examples: --collision source:always");
    println!("                                  --collision source:newer,larger");
    println!("                                  --collision dest:newer+larger");
    println!("  -L depth              Max depth of preview tree (default: 1).");
    println!("  -T trunc              Max entries per folder before truncation (default: 25).");
    println!("  --replace-dest-symlink");
    println!(
        "                        Replace a destination symlink itself instead of following it."
    );
    println!("  --preview             Run preview only (no prompt, no transfer).");
    println!("  --preview-lite        Faster preview-only mode; skips exact byte scan on brand-new destination trees.");
    println!(
        "  --                    End options; paths beginning with '-' are treated as operands."
    );
    println!();
    println!("decision tree:");
    println!(
        "{}",
        r#"normal_copy(S, D, options)
  |
  |
  v
Resolve effective target T
  |
  +-- D exists as directory?
  |       |
  |       +-- yes (directory):
  |       |       T = D/basename(S)
  |       |       D is a container destination
  |       |
  |       +-- no (either missing or a file):
  |               T = D
  |               D is an exact-path destination
  |
  v
What is S?
  +-- does not exist
  |   error
  |
  +-- existing file
  |   |
  |   v
  |  What is T?
  |    +-- missing
  |    |   create file at T
  |    +-- existing file
  |    |   apply file collision policy:
  |    |     default: --collision source:size-differs
  |    |       if size(S) != size(T):
  |    |         replace T with S
  |    |       else:
  |    |         keep T
  |    |     other examples:
  |    |       --collision source:always
  |    |       --collision source:newer,larger
  |    |       --collision dest:newer+larger
  |    +-- existing directory
  |        error
  |
  +-- existing directory
      |
      v
     What is T?
       +-- missing
       |   create directory T
       |   copy contents of S into T
       +-- file
       |   error
       +-- directory
           |
           v
          Is -o enabled?
            +-- no
            |   merge S into T recursively
            |   for every child: apply this same decision tree
            |   if source child file maps to existing target child file:
            |     apply file collision policy
            |   if -b is set:
            |     back up existing target directory T before merge starts
            +-- yes
                overwrite T with S
                if -b: back up existing directory T first
                replace T with a copy of S"#
    );
}

pub(crate) fn parse_args() -> Result<CliArgs, i32> {
    let mut args = CliArgs {
        tree_trunc: 25,
        ..CliArgs::default()
    };
    let mut positional: Vec<String> = Vec::new();
    let argv: Vec<OsString> = env::args_os().skip(1).collect();
    let mut i = 0usize;
    let mut options_done = false;

    while i < argv.len() {
        let raw = match argv[i].to_str() {
            Some(raw) => raw,
            None => {
                usage();
                eprintln!("copy: error: non-UTF-8 command-line paths are not supported; refusing to continue");
                return Err(1);
            }
        };
        if options_done {
            positional.push(raw.to_string());
            i += 1;
            continue;
        }
        match raw {
            "--" => options_done = true,
            "-h" | "--help" => {
                print_help();
                return Err(0);
            }
            "-m" | "--move" => args.move_mode = true,
            "-s" | "--sudo" => args.sudo = true,
            "-o" | "--overwrite" => args.overwrite = true,
            "-c" | "--contents-only" => args.contents_only = true,
            "--create-destination-parents" => args.create_destination_parents = true,
            "-b" | "--backup" => args.backup = true,
            "--sync" => args.sync_mode = true,
            "-v" | "--verbose" | "--showall" => args.showall = true,
            "--replace-dest-symlink" => args.replace_dest_symlink = true,
            "--collision" => {
                i += 1;
                if i >= argv.len() {
                    usage();
                    eprintln!("copy: error: --collision requires an argument");
                    return Err(1);
                }
                let value = match argv[i].to_str() {
                    Some(value) => value,
                    None => {
                        usage();
                        eprintln!("copy: error: --collision value is not valid UTF-8");
                        return Err(1);
                    }
                };
                let policy = match parse_merge_collision_policy(value) {
                    Ok(v) => v,
                    Err(msg) => {
                        usage();
                        eprintln!("copy: error: invalid --collision value '{}': {msg}", value);
                        return Err(1);
                    }
                };
                set_merge_collision_policy(&mut args, policy)?
            }
            "-L" => {
                i += 1;
                if i >= argv.len() {
                    usage();
                    eprintln!("copy: error: -L requires an argument");
                    return Err(1);
                }
                match argv[i]
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                {
                    Some(v) => args.tree_depth = Some(v),
                    None => {
                        usage();
                        eprintln!("copy: error: -L argument must be a positive integer");
                        return Err(1);
                    }
                }
            }
            "-T" => {
                i += 1;
                if i >= argv.len() {
                    usage();
                    eprintln!("copy: error: -T requires an argument");
                    return Err(1);
                }
                match argv[i]
                    .to_str()
                    .and_then(|value| value.parse::<usize>().ok())
                {
                    Some(v) => args.tree_trunc = v,
                    None => {
                        usage();
                        eprintln!("copy: error: -T argument must be a positive integer");
                        return Err(1);
                    }
                }
            }
            "--preview" => args.preview_only = true,
            "--preview-lite" => args.preview_lite = true,
            _ if raw.starts_with('-') => {
                usage();
                eprintln!("copy: error: unrecognized arguments: {raw}");
                return Err(1);
            }
            _ => positional.push(raw.to_string()),
        }
        i += 1;
    }

    if positional.len() < 2 {
        usage();
        eprintln!("copy: error: the following arguments are required: source, destination");
        return Err(1);
    }

    args.source = positional.remove(0);
    args.destination = positional.pop().unwrap_or_default();
    args.extra = positional;
    Ok(args)
}
