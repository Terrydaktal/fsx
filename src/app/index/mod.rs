use super::*;

use memmap2::Mmap;
use rusqlite::{params, params_from_iter, Connection, OpenFlags};
use std::borrow::Cow;
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
#[cfg(feature = "watcher")]
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jwalk::{Parallelism, WalkDir};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;

mod protocol;
mod query;
mod refresh;
mod snapshot;
mod storage;

pub(crate) use protocol::*;
pub(crate) use query::*;
pub(crate) use refresh::*;
pub(crate) use snapshot::*;
pub(crate) use storage::*;
