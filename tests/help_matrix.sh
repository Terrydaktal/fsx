#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
F_BIN="${F_BIN:-${ROOT_DIR}/target/release/unearth}"
F_TIMEOUT="${F_TIMEOUT:-20}"

if [[ ! -x "$F_BIN" || "${ROOT_DIR}/src/main.rs" -nt "$F_BIN" || "${ROOT_DIR}/Cargo.toml" -nt "$F_BIN" ]]; then
  cargo build --release --quiet --manifest-path "${ROOT_DIR}/Cargo.toml"
fi

F="$F_BIN"

assert_eq() {
  local name="$1"
  local got="$2"
  local want="$3"
  if [[ "$got" != "$want" ]]; then
    echo "FAIL: ${name}" >&2
    echo "----- got -----" >&2
    printf '%s\n' "$got" >&2
    echo "----- want ----" >&2
    printf '%s\n' "$want" >&2
    exit 1
  fi
}

assert_contains() {
  local name="$1"
  local haystack="$2"
  local needle="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    echo "FAIL: ${name}" >&2
    echo "----- got -----" >&2
    printf '%s\n' "$haystack" >&2
    echo "----- missing ----" >&2
    printf '%s\n' "$needle" >&2
    exit 1
  fi
}

assert_not_contains() {
  local name="$1"
  local haystack="$2"
  local needle="$3"
  if [[ "$haystack" == *"$needle"* ]]; then
    echo "FAIL: ${name}" >&2
    echo "----- got -----" >&2
    printf '%s\n' "$haystack" >&2
    echo "----- unexpected ----" >&2
    printf '%s\n' "$needle" >&2
    exit 1
  fi
}

assert_regex() {
  local name="$1"
  local text="$2"
  local pattern="$3"
  if ! printf '%s\n' "$text" | grep -Eq "$pattern"; then
    echo "FAIL: ${name}" >&2
    echo "----- got -----" >&2
    printf '%s\n' "$text" >&2
    echo "----- pattern ----" >&2
    printf '%s\n' "$pattern" >&2
    exit 1
  fi
}

list_rel() {
  local root="$1"
  shift
  "$F" --timeout "$F_TIMEOUT" "$@" "$root" 2>/dev/null | sed "s#^${root}/##" | sort
}

list_rel_raw() {
  local root="$1"
  shift
  "$F" --timeout "$F_TIMEOUT" "$@" "$root" 2>/dev/null | sed "s#^${root}/##"
}

list_parent_dirs() {
  "$F" --timeout "$F_TIMEOUT" "$@" 2>/dev/null | xargs -r -n1 dirname | sort -u
}

TMP_BASE="/tmp/unearth_help_matrix_${RANDOM}_$$"
FILE_ROOT="${TMP_BASE}/file_root"
DIR_ROOT="${TMP_BASE}/dir_root"
SD_BASE="${TMP_BASE}/search_dir_root"
TOKEN="sdtok_${RANDOM}_$$"
NEEDLE="needle_${RANDOM}_$$"
trap 'rm -rf "$TMP_BASE"' EXIT

mkdir -p "$FILE_ROOT" "$DIR_ROOT"
touch "${FILE_ROOT}/abc" "${FILE_ROOT}/xabc" "${FILE_ROOT}/abcx" "${FILE_ROOT}/xabcx" "${FILE_ROOT}/x.tecneq"
mkdir -p "${DIR_ROOT}/abc" "${DIR_ROOT}/xabc" "${DIR_ROOT}/abcx" "${DIR_ROOT}/xabcx"

want_contains=$'abc\nabcx\nxabc\nxabcx'
want_starts=$'abc\nabcx'
want_ends=$'abc\nxabc'
want_exact='abc'
want_contains_d=$'abc/\nabcx/\nxabc/\nxabcx/'
want_starts_d=$'abc/\nabcx/'
want_ends_d=$'abc/\nxabc/'
want_exact_d='abc/'

# SEARCH MATRIX: contains
assert_eq "contains all (shorthand files)" "$(list_rel "$FILE_ROOT" abc)" "$want_contains"
assert_eq "contains all (wildcard files)" "$(list_rel "$FILE_ROOT" '*abc*')" "$want_contains"
assert_eq "contains all (regex files)" "$(list_rel "$FILE_ROOT" -r 'abc')" "$want_contains"
assert_eq "contains file (shorthand)" "$(list_rel "$FILE_ROOT" abc -f)" "$want_contains"
assert_eq "contains file (wildcard)" "$(list_rel "$FILE_ROOT" '*abc*' -f)" "$want_contains"
assert_eq "contains file (regex)" "$(list_rel "$FILE_ROOT" -r 'abc' -f)" "$want_contains"
assert_eq "contains all (shorthand dirs)" "$(list_rel "$DIR_ROOT" abc)" "$want_contains_d"
assert_eq "contains all (wildcard dirs)" "$(list_rel "$DIR_ROOT" '*abc*')" "$want_contains_d"
assert_eq "contains all (regex dirs)" "$(list_rel "$DIR_ROOT" -r 'abc')" "$want_contains_d"
assert_eq "contains dir (shorthand)" "$(list_rel "$DIR_ROOT" abc -d)" "$want_contains_d"
assert_eq "contains dir (wildcard)" "$(list_rel "$DIR_ROOT" '*abc*' -d)" "$want_contains_d"
assert_eq "contains dir (regex)" "$(list_rel "$DIR_ROOT" -r 'abc' -d)" "$want_contains_d"
assert_eq "legacy r-prefix is literal without -r" "$(list_rel "$FILE_ROOT" 'r.tecneq')" ""
assert_eq "contains regex dot (flag mode)" "$(list_rel "$FILE_ROOT" -r '.tecneq$')" "x.tecneq"

# SEARCH MATRIX: exact
assert_eq "exact all (wildcard format files)" "$(list_rel "$FILE_ROOT" '"abc"')" "$want_exact"
assert_eq "exact all (regex files)" "$(list_rel "$FILE_ROOT" -r '^abc$')" "$want_exact"
assert_eq "exact file (wildcard format)" "$(list_rel "$FILE_ROOT" '"abc"' -f)" "$want_exact"
assert_eq "exact file (regex)" "$(list_rel "$FILE_ROOT" -r '^abc$' -f)" "$want_exact"
assert_eq "exact all (wildcard format dirs)" "$(list_rel "$DIR_ROOT" '"abc"')" "$want_exact_d"
assert_eq "exact all (regex dirs)" "$(list_rel "$DIR_ROOT" -r '^abc$')" "$want_exact_d"
assert_eq "exact dir (shorthand)" "$(list_rel "$DIR_ROOT" /abc/)" "$want_exact_d"
assert_eq "exact dir (wildcard format)" "$(list_rel "$DIR_ROOT" '"abc"' -d)" "$want_exact_d"
assert_eq "exact dir (regex)" "$(list_rel "$DIR_ROOT" -r '^abc$' -d)" "$want_exact_d"

# SEARCH MATRIX: starts
assert_eq "starts all (shorthand files)" "$(list_rel "$FILE_ROOT" /abc)" "$want_starts"
assert_eq "starts all (wildcard files)" "$(list_rel "$FILE_ROOT" 'abc*')" "$want_starts"
assert_eq "starts all (regex files)" "$(list_rel "$FILE_ROOT" -r '^abc')" "$want_starts"
assert_eq "starts file (shorthand)" "$(list_rel "$FILE_ROOT" /abc -f)" "$want_starts"
assert_eq "starts file (wildcard)" "$(list_rel "$FILE_ROOT" 'abc*' -f)" "$want_starts"
assert_eq "starts file (regex)" "$(list_rel "$FILE_ROOT" -r '^abc' -f)" "$want_starts"
assert_eq "starts all (shorthand dirs)" "$(list_rel "$DIR_ROOT" /abc)" "$want_starts_d"
assert_eq "starts all (wildcard dirs)" "$(list_rel "$DIR_ROOT" 'abc*')" "$want_starts_d"
assert_eq "starts all (regex dirs)" "$(list_rel "$DIR_ROOT" -r '^abc')" "$want_starts_d"
assert_eq "starts dir (shorthand)" "$(list_rel "$DIR_ROOT" /abc -d)" "$want_starts_d"
assert_eq "starts dir (wildcard)" "$(list_rel "$DIR_ROOT" 'abc*' -d)" "$want_starts_d"
assert_eq "starts dir (regex)" "$(list_rel "$DIR_ROOT" -r '^abc' -d)" "$want_starts_d"

# SEARCH MATRIX: ends
assert_eq "ends all (wildcard files)" "$(list_rel "$FILE_ROOT" '*abc')" "$want_ends"
assert_eq "ends all (regex files)" "$(list_rel "$FILE_ROOT" -r 'abc$')" "$want_ends"
assert_eq "ends file (wildcard)" "$(list_rel "$FILE_ROOT" '*abc' -f)" "$want_ends"
assert_eq "ends file (regex)" "$(list_rel "$FILE_ROOT" -r 'abc$' -f)" "$want_ends"
assert_eq "ends all (wildcard dirs)" "$(list_rel "$DIR_ROOT" '*abc')" "$want_ends_d"
assert_eq "ends all (regex dirs)" "$(list_rel "$DIR_ROOT" -r 'abc$')" "$want_ends_d"
assert_eq "ends dir (shorthand)" "$(list_rel "$DIR_ROOT" abc/)" "$want_ends_d"
assert_eq "ends dir (wildcard)" "$(list_rel "$DIR_ROOT" '*abc' -d)" "$want_ends_d"
assert_eq "ends dir (regex)" "$(list_rel "$DIR_ROOT" -r 'abc$' -d)" "$want_ends_d"

# SEARCH DIR MATRIX setup
mkdir -p "$SD_BASE"
SD_EXACT="${SD_BASE}/${TOKEN}"
SD_CONTAINS="${SD_BASE}/pre_${TOKEN}_mid"
SD_STARTS="${SD_BASE}/${TOKEN}_start"
SD_ENDS="${SD_BASE}/end_${TOKEN}"
mkdir -p "$SD_EXACT" "$SD_CONTAINS" "$SD_STARTS" "$SD_ENDS"
touch "${SD_EXACT}/${NEEDLE}" "${SD_CONTAINS}/${NEEDLE}" "${SD_STARTS}/${NEEDLE}" "${SD_ENDS}/${NEEDLE}"

want_sd_contains=$(printf '%s\n' "$SD_CONTAINS" "$SD_ENDS" "$SD_EXACT" "$SD_STARTS" | sort)
want_sd_exact=$(printf '%s\n' "$SD_EXACT")
want_sd_starts=$(printf '%s\n' "$SD_EXACT" "$SD_STARTS" | sort)
want_sd_ends=$(printf '%s\n' "$SD_ENDS" "$SD_EXACT" | sort)

# SEARCH DIR MATRIX: contains
assert_eq "search_dir contains (shorthand)" "$(list_parent_dirs "\"${NEEDLE}\"" "$TOKEN")" "$want_sd_contains"
assert_eq "search_dir contains (wildcard)" "$(list_parent_dirs "\"${NEEDLE}\"" "*${TOKEN}*")" "$want_sd_contains"
assert_eq "search_dir contains (regex)" "$(list_parent_dirs -r "\"${NEEDLE}\"" "${TOKEN}")" "$want_sd_contains"

# SEARCH DIR MATRIX: exact
assert_eq "search_dir exact (shorthand)" "$(list_parent_dirs "\"${NEEDLE}\"" "/${TOKEN}/")" "$want_sd_exact"
assert_eq "search_dir exact (regex)" "$(list_parent_dirs -r "\"${NEEDLE}\"" "^${TOKEN}\$")" "$want_sd_exact"

# SEARCH DIR MATRIX: starts
assert_eq "search_dir starts (shorthand)" "$(list_parent_dirs "\"${NEEDLE}\"" "/${TOKEN}")" "$want_sd_starts"
assert_eq "search_dir starts (wildcard)" "$(list_parent_dirs "\"${NEEDLE}\"" "\"${TOKEN}*\"")" "$want_sd_starts"
assert_eq "search_dir starts (regex)" "$(list_parent_dirs -r "\"${NEEDLE}\"" "^${TOKEN}")" "$want_sd_starts"

# SEARCH DIR MATRIX: ends
assert_eq "search_dir ends (shorthand)" "$(list_parent_dirs "\"${NEEDLE}\"" "${TOKEN}/")" "$want_sd_ends"
assert_eq "search_dir ends (wildcard)" "$(list_parent_dirs "\"${NEEDLE}\"" "\"*${TOKEN}\"")" "$want_sd_ends"
assert_eq "search_dir ends (regex)" "$(list_parent_dirs -r "\"${NEEDLE}\"" "${TOKEN}\$")" "$want_sd_ends"

# SORT MATRIX: date
SORT_ROOT="${TMP_BASE}/sort_root"
mkdir -p "$SORT_ROOT"
touch -d '2020-01-01 00:00:00 UTC' "${SORT_ROOT}/z_old"
touch -d '2021-01-01 00:00:00 UTC' "${SORT_ROOT}/m_mid"
touch -d '2022-01-01 00:00:00 UTC' "${SORT_ROOT}/a_new"

want_sort_asc=$'z_old\nm_mid\na_new'
want_sort_desc=$'a_new\nm_mid\nz_old'
assert_eq "sort date asc" "$(list_rel_raw "$SORT_ROOT" --sort date asc '*')" "$want_sort_asc"
assert_eq "sort date desc" "$(list_rel_raw "$SORT_ROOT" --sort date desc '*')" "$want_sort_desc"

# SORT MATRIX: size
SIZE_ROOT="${TMP_BASE}/size_root"
mkdir -p "$SIZE_ROOT"
truncate -s 1 "${SIZE_ROOT}/b_small"
truncate -s 128 "${SIZE_ROOT}/c_mid"
truncate -s 4096 "${SIZE_ROOT}/a_big"

want_size_asc=$'b_small\nc_mid\na_big'
want_size_desc=$'a_big\nc_mid\nb_small'
assert_eq "sort size asc" "$(list_rel_raw "$SIZE_ROOT" --sort size asc '*')" "$want_size_asc"
assert_eq "sort size desc" "$(list_rel_raw "$SIZE_ROOT" --sort size desc '*')" "$want_size_desc"

# INDEX DATABASE
INDEX_ROOT="${TMP_BASE}/index_root"
INDEX_CACHE="${TMP_BASE}/index_cache"
mkdir -p "${INDEX_ROOT}/a/sub" "${INDEX_ROOT}/.hidden"
touch "${INDEX_ROOT}/a/file.txt" "${INDEX_ROOT}/a/sub/nested.txt" "${INDEX_ROOT}/.hidden/secret.txt"
XDG_CACHE_HOME="$INDEX_CACHE" "$F" --index-refresh "$INDEX_ROOT"
want_index_all=$(printf '%s\n' \
  "${INDEX_ROOT}/.hidden/" \
  "${INDEX_ROOT}/.hidden/secret.txt" \
  "${INDEX_ROOT}/a/" \
  "${INDEX_ROOT}/a/file.txt" \
  "${INDEX_ROOT}/a/sub/" \
  "${INDEX_ROOT}/a/sub/nested.txt" | sort)
want_index_visible=$(printf '%s\n' \
  "${INDEX_ROOT}/a/" \
  "${INDEX_ROOT}/a/file.txt" \
  "${INDEX_ROOT}/a/sub/" \
  "${INDEX_ROOT}/a/sub/nested.txt" | sort)
want_index_dirs=$(printf '%s\n' \
  "${INDEX_ROOT}/.hidden/" \
  "${INDEX_ROOT}/a/" \
  "${INDEX_ROOT}/a/sub/" | sort)
want_index_visible_dirs=$(printf '%s\n' \
  "${INDEX_ROOT}/a/" \
  "${INDEX_ROOT}/a/sub/" | sort)
assert_eq "index all includes hidden" "$(XDG_CACHE_HOME="$INDEX_CACHE" "$F" --index '*' -H --color=never "$INDEX_ROOT" | sort)" "$want_index_all"
assert_eq "index visible excludes hidden" "$(XDG_CACHE_HOME="$INDEX_CACHE" "$F" --index '*' --color=never "$INDEX_ROOT" | sort)" "$want_index_visible"
assert_eq "index dirs only" "$(XDG_CACHE_HOME="$INDEX_CACHE" "$F" --index '*' -d -H --color=never "$INDEX_ROOT" | sort)" "$want_index_dirs"
assert_eq "index visible dirs only" "$(XDG_CACHE_HOME="$INDEX_CACHE" "$F" --index '*' -d --color=never "$INDEX_ROOT" | sort)" "$want_index_visible_dirs"
assert_eq "index trigram substring lookup" "$(XDG_CACHE_HOME="$INDEX_CACHE" "$F" --index nested -H --color=never "$INDEX_ROOT")" "${INDEX_ROOT}/a/sub/nested.txt"
index_db="${INDEX_CACHE}/unearth/index/unearth.db"
assert_eq "index records exact refreshed root" "$(sqlite3 "$index_db" "select count(*) from indexed_roots where root='${INDEX_ROOT}';")" "1"
assert_eq "index child root does not mark parent refreshed" "$(sqlite3 "$index_db" "select count(*) from indexed_roots where root='${TMP_BASE}';")" "0"
XDG_CACHE_HOME="$INDEX_CACHE" "$F" --index-purge "${INDEX_ROOT}/a/sub"
assert_eq "index purge removes child dirs" "$(sqlite3 "$index_db" "select count(*) from dirs where path='${INDEX_ROOT}/a/sub';")" "0"
assert_eq "index purge keeps sibling entries" "$(sqlite3 "$index_db" "select count(*) from entries e join dirs d on e.dir_id=d.id join strings s on e.name_id=s.id where d.path='${INDEX_ROOT}/a' and s.value='file.txt';")" "1"
assert_eq "index purge invalidates containing root" "$(sqlite3 "$index_db" "select count(*) from indexed_roots where root='${INDEX_ROOT}';")" "0"

# SORT MATRIX: size (directories by real size)
SIZE_DIR_ROOT="${TMP_BASE}/size_dir_root"
mkdir -p "${SIZE_DIR_ROOT}/big_dir" "${SIZE_DIR_ROOT}/small_dir"
dd if=/dev/zero of="${SIZE_DIR_ROOT}/big_dir/blob" bs=1024 count=1024 status=none
dd if=/dev/zero of="${SIZE_DIR_ROOT}/small_dir/tiny" bs=1024 count=1 status=none
want_size_dir_desc=$'big_dir/\nsmall_dir/'
assert_eq "sort size desc dirs" "$(list_rel_raw "$SIZE_DIR_ROOT" --sort size desc -d '*')" "$want_size_dir_desc"

# SIZES MATRIX: compact display values
SIZES_DIR_ROOT="${TMP_BASE}/sizes_dir_root"
mkdir -p "${SIZES_DIR_ROOT}/big_dir" "${SIZES_DIR_ROOT}/small_dir"
dd if=/dev/zero of="${SIZES_DIR_ROOT}/big_dir/blob" bs=1024 count=2 status=none
dd if=/dev/zero of="${SIZES_DIR_ROOT}/small_dir/tiny" bs=1024 count=1 status=none
sizes_dir_desc="$("$F" --timeout "$F_TIMEOUT" --sizes --sort size desc -d '*' "$SIZES_DIR_ROOT" 2>/dev/null | sed "s#\t${SIZES_DIR_ROOT}/#\t#")"
want_sizes_dir_desc=$'2.000K\tbig_dir/\n1.000K\tsmall_dir/'
assert_eq "sizes reports recursive dir values" "$sizes_dir_desc" "$want_sizes_dir_desc"

SIZES_FILE_ROOT="${TMP_BASE}/sizes_file_root"
mkdir -p "$SIZES_FILE_ROOT"
dd if=/dev/zero of="${SIZES_FILE_ROOT}/large.bin" bs=1 count=512 status=none
dd if=/dev/zero of="${SIZES_FILE_ROOT}/small.bin" bs=1 count=128 status=none
sizes_file_desc="$("$F" --timeout "$F_TIMEOUT" --sizes --sort size desc -f '*' "$SIZES_FILE_ROOT" 2>/dev/null | sed "s#\t${SIZES_FILE_ROOT}/#\t#")"
want_sizes_file_desc=$'512B\tlarge.bin\n128B\tsmall.bin'
assert_eq "sizes reports file values" "$sizes_file_desc" "$want_sizes_file_desc"

# SORT MATRIX: name
NAME_ROOT="${TMP_BASE}/name_root"
mkdir -p "$NAME_ROOT"
touch "${NAME_ROOT}/Zulu" "${NAME_ROOT}/alpha" "${NAME_ROOT}/Beta"

want_name_asc=$'alpha\nBeta\nZulu'
want_name_desc=$'Zulu\nBeta\nalpha'
assert_eq "sort name asc" "$(list_rel_raw "$NAME_ROOT" --sort name asc '*')" "$want_name_asc"
assert_eq "sort name desc" "$(list_rel_raw "$NAME_ROOT" --sort name desc '*')" "$want_name_desc"

# RECURSION MATRIX
NONREC_ROOT="${TMP_BASE}/nonrec_root"
mkdir -p "${NONREC_ROOT}/sub"
touch "${NONREC_ROOT}/abc_top" "${NONREC_ROOT}/sub/abc_nested"

want_recursive=$'abc_top\nsub/abc_nested'
want_non_recursive='abc_top'
assert_eq "recursive default" "$(list_rel "$NONREC_ROOT" abc -f)" "$want_recursive"
assert_eq "no recurse" "$(list_rel "$NONREC_ROOT" abc -f --no-recurse)" "$want_non_recursive"
assert_eq "no recurse alias -R" "$(list_rel "$NONREC_ROOT" abc -f -R)" "$want_non_recursive"
assert_eq "no recurse+file combined short -Rf" "$(list_rel "$NONREC_ROOT" abc -Rf)" "$want_non_recursive"
assert_eq "threads flag (space form)" "$(list_rel "$NONREC_ROOT" abc -f --threads 1)" "$want_recursive"
assert_eq "threads flag (equals form)" "$(list_rel "$NONREC_ROOT" abc -f --threads=1)" "$want_recursive"

cluster_full_sep="$("$F" --timeout "$F_TIMEOUT" -F -H abc "$FILE_ROOT" 2>/dev/null | sed "s#^${FILE_ROOT}/##" | sort)"
cluster_full_combined="$("$F" --timeout "$F_TIMEOUT" -FH abc "$FILE_ROOT" 2>/dev/null | sed "s#^${FILE_ROOT}/##" | sort)"
assert_eq "combined short -FH equals separated -F -H" "$cluster_full_combined" "$cluster_full_sep"

FULL_DESC_ROOT="${TMP_BASE}/full_desc_root"
mkdir -p "${FULL_DESC_ROOT}/abc_parent/sub"
touch "${FULL_DESC_ROOT}/abc_parent/sub/abc_child.txt"
full_desc_out="$(list_rel "$FULL_DESC_ROOT" -F abc)"
assert_contains "full mode keeps matching descendants under matching parent dirs" "$full_desc_out" "abc_parent/sub/abc_child.txt"

FULL_BASENAME_ROOT="${TMP_BASE}/full_basename_root"
mkdir -p "${FULL_BASENAME_ROOT}/screenshot/deps"
touch "${FULL_BASENAME_ROOT}/screenshot/deps/libqoi-ec2c782670acca15.rmeta"
touch "${FULL_BASENAME_ROOT}/deps_screenshot.txt"
full_basename_out="$(list_rel "$FULL_BASENAME_ROOT" -F screenshot)"
assert_contains "full mode single-term includes matching dir basename" "$full_basename_out" "screenshot/"
assert_contains "full mode single-term includes matching file basename" "$full_basename_out" "deps_screenshot.txt"
assert_not_contains "full mode single-term excludes non-matching descendant basenames" "$full_basename_out" "screenshot/deps/libqoi-ec2c782670acca15.rmeta"

# HIGHLIGHT MATRIX
HIGHLIGHT_ROOT="${TMP_BASE}/highlight_root"
mkdir -p "$HIGHLIGHT_ROOT"
touch "${HIGHLIGHT_ROOT}/screenshot.txt"
highlight_out="$("$F" --timeout "$F_TIMEOUT" --color=never --highlight-match screenshot "$HIGHLIGHT_ROOT" 2>/dev/null)"
assert_contains "highlight flag wraps matched text in bold bright red" "$highlight_out" $'\033[1;91mscreenshot\033[0m'

# ABSOLUTE OUTPUT MATRIX
ABS_ROOT="${TMP_BASE}/abs_root"
mkdir -p "$ABS_ROOT"
touch "${ABS_ROOT}/absolute_probe"
assert_regex "default output is relative paths" "$(cd "$ABS_ROOT" && "$F" --timeout "$F_TIMEOUT" absolute_probe -f 2>/dev/null)" '^(\./)?absolute_probe$'
assert_eq "absolute paths output with -A" "$(cd "$ABS_ROOT" && "$F" --timeout "$F_TIMEOUT" absolute_probe -f -A 2>/dev/null)" "${ABS_ROOT}/absolute_probe"
assert_eq "absolute paths output with --absolute-paths" "$(cd "$ABS_ROOT" && "$F" --timeout "$F_TIMEOUT" absolute_probe -f --absolute-paths 2>/dev/null)" "${ABS_ROOT}/absolute_probe"

# CLASSIFY MATRIX
CLASSIFY_ROOT="${TMP_BASE}/classify_root"
mkdir -p "$CLASSIFY_ROOT"
touch "${CLASSIFY_ROOT}/target_file"
ln -s target_file "${CLASSIFY_ROOT}/link_file"
classify_default="$("$F" --timeout "$F_TIMEOUT" link_file -f "$CLASSIFY_ROOT" 2>/dev/null | sed "s#^${CLASSIFY_ROOT}/##")"
classify_forced="$("$F" --timeout "$F_TIMEOUT" link_file -f -C "$CLASSIFY_ROOT" 2>/dev/null | sed "s#^${CLASSIFY_ROOT}/##")"
classify_combined="$("$F" --timeout "$F_TIMEOUT" link_file -Cf "$CLASSIFY_ROOT" 2>/dev/null | sed "s#^${CLASSIFY_ROOT}/##")"
assert_eq "classify default off on non-tty" "$classify_default" "link_file"
assert_eq "classify enabled by -C" "$classify_forced" "link_file@"
assert_eq "classify enabled by combined -Cf" "$classify_combined" "link_file@"

# COUNTS MATRIX
COUNTS_ROOT="${TMP_BASE}/counts_root"
mkdir -p "${COUNTS_ROOT}/a" "${COUNTS_ROOT}/b" "${COUNTS_ROOT}/c"
touch "${COUNTS_ROOT}/a/hit1" "${COUNTS_ROOT}/a/hit2"
touch "${COUNTS_ROOT}/b/hit1" "${COUNTS_ROOT}/b/hit2" "${COUNTS_ROOT}/b/hit3"
touch "${COUNTS_ROOT}/c/hit1"
counts_out="$("$F" --timeout "$F_TIMEOUT" hit "$COUNTS_ROOT" --counts --color=never 2>/dev/null | awk '{print $1 "\t" $2}')"
want_counts_out=$'1\t'"${COUNTS_ROOT}/c"$'\n2\t'"${COUNTS_ROOT}/a"$'\n3\t'"${COUNTS_ROOT}/b"
assert_eq "counts default sort is ascending by count then folder" "$counts_out" "$want_counts_out"
counts_color_out="$("$F" --timeout "$F_TIMEOUT" hit "$COUNTS_ROOT" --counts --color=always 2>/dev/null)"
assert_contains "counts output colors folder column when requested" "$counts_color_out" $'\033['
counts_link_out="$("$F" --timeout "$F_TIMEOUT" hit "$COUNTS_ROOT" --counts --color=always --hyperlink 2>/dev/null)"
assert_contains "counts output hyperlinks folder column when requested" "$counts_link_out" $'\033]8;;file://'

threads_err="$("$F" --timeout "$F_TIMEOUT" --threads 0 abc "$NONREC_ROOT" 2>&1 >/dev/null || true)"
assert_contains "threads invalid value errors" "$threads_err" "--threads requires a positive integer"

# CACHE-RAW MATRIX
CACHE_USER="unearth_cache_test_${RANDOM}_$$"
CACHE_FISH_PID="424242"
CACHE_ROOT="/tmp/fzf-history-${CACHE_USER}"
rm -rf "$CACHE_ROOT"
cache_raw_out="$(USER="$CACHE_USER" FISH_PID="$CACHE_FISH_PID" "$F" --timeout "$F_TIMEOUT" --cache-raw abc -f "$FILE_ROOT" 2>/dev/null | sed "s#^${FILE_ROOT}/##" | sort)"
assert_eq "cache-raw output unchanged" "$cache_raw_out" "$want_contains"
cache_raw_dirs_file="${CACHE_ROOT}/universal-last-dirs-${CACHE_FISH_PID}"
cache_raw_files_file="${CACHE_ROOT}/universal-last-files-${CACHE_FISH_PID}"
cache_raw_saved_files="$(sed "s#^${FILE_ROOT}/##" "$cache_raw_files_file" | sort)"
assert_eq "cache-raw writes files cache" "$cache_raw_saved_files" "$want_contains"
cache_raw_saved_dirs_from_file="$(sort "$cache_raw_dirs_file")"
assert_eq "cache-raw writes parent dir for file search" "$cache_raw_saved_dirs_from_file" "${FILE_ROOT}/"
USER="$CACHE_USER" FISH_PID="$CACHE_FISH_PID" "$F" --timeout "$F_TIMEOUT" --cache-raw abc -d "$DIR_ROOT" >/dev/null 2>&1
cache_raw_saved_dirs="$(sort "$cache_raw_dirs_file")"
want_cache_raw_dirs=$(printf '%s\n' "${DIR_ROOT}/" "${DIR_ROOT}/abc/" "${DIR_ROOT}/abcx/" "${DIR_ROOT}/xabc/" "${DIR_ROOT}/xabcx/" | sort)
assert_eq "cache-raw writes dirs cache (matches + parent)" "$cache_raw_saved_dirs" "$want_cache_raw_dirs"
assert_eq "cache-raw files cache empty for dir search" "$(cat "$cache_raw_files_file")" ""
rm -rf "$CACHE_ROOT"
cache_legacy_err="$("$F" --timeout "$F_TIMEOUT" --cache abc "$FILE_ROOT" 2>&1 >/dev/null || true)"
assert_contains "legacy cache flag errors" "$cache_legacy_err" "--cache was renamed to --cache-raw"

# VISIBILITY MATRIX
VISIBLE_ROOT="${TMP_BASE}/visible_root"
mkdir -p "$VISIBLE_ROOT"
touch "${VISIBLE_ROOT}/.hidden_hit" "${VISIBLE_ROOT}/visible_hit"
want_visible_default='visible_hit'
want_visible_all=$'.hidden_hit\nvisible_hit'
assert_eq "default excludes hidden entries" "$(list_rel "$VISIBLE_ROOT" '*hit' -f)" "$want_visible_default"
assert_eq "hidden flag includes hidden entries" "$(list_rel "$VISIBLE_ROOT" '*hit' -f --hidden)" "$want_visible_all"

# IGNORE MATRIX
IGNORE_ROOT="${TMP_BASE}/ignore_root"
mkdir -p "$IGNORE_ROOT"
touch "${IGNORE_ROOT}/ignore_me" "${IGNORE_ROOT}/keep_me"
printf 'ignore_me\n' > "${IGNORE_ROOT}/.gitignore"
git -C "$IGNORE_ROOT" init -q
want_ignore_default='ignore_me'
assert_eq "default bypasses gitignore" "$(list_rel "$IGNORE_ROOT" ignore_me -f)" "$want_ignore_default"
assert_eq "ignore respects gitignore" "$(list_rel "$IGNORE_ROOT" ignore_me -f --ignore)" ""

# FOLLOW-LINKS MATRIX
FOLLOW_ROOT="${TMP_BASE}/follow_root"
FOLLOW_EXTERNAL="${TMP_BASE}/follow_external"
mkdir -p "$FOLLOW_ROOT" "$FOLLOW_EXTERNAL"
touch "${FOLLOW_EXTERNAL}/follow_only"
ln -s "$FOLLOW_EXTERNAL" "${FOLLOW_ROOT}/linked_dir"
assert_eq "no follow-links does not traverse symlinked dirs" "$(list_rel "$FOLLOW_ROOT" follow_only -f)" ""
assert_eq "follow-links traverses symlinked dirs" "$(list_rel "$FOLLOW_ROOT" follow_only -f --follow-links)" "linked_dir/follow_only"

# RECURSION + SIZE SORT MATRIX (fast non-recursive size key for dirs)
NONREC_SIZE_ROOT="${TMP_BASE}/nonrec_size_root"
mkdir -p "${NONREC_SIZE_ROOT}/huge_dir"
dd if=/dev/zero of="${NONREC_SIZE_ROOT}/huge_dir/blob" bs=1024 count=1024 status=none
dd if=/dev/zero of="${NONREC_SIZE_ROOT}/top_file" bs=1024 count=64 status=none
want_nonrec_size_desc=$'top_file\nhuge_dir/'
assert_eq "no recurse size sort desc uses direct entry size" "$(list_rel_raw "$NONREC_SIZE_ROOT" --sort size desc -R '*')" "$want_nonrec_size_desc"

NONREC_SIZE_L_ROOT="${TMP_BASE}/nonrec_size_l_root"
mkdir -p "${NONREC_SIZE_L_ROOT}/big_dir" "${NONREC_SIZE_L_ROOT}/small_dir"
dd if=/dev/zero of="${NONREC_SIZE_L_ROOT}/big_dir/blob" bs=1024 count=1024 status=none
dd if=/dev/zero of="${NONREC_SIZE_L_ROOT}/small_dir/tiny" bs=1024 count=1 status=none
want_nonrec_size_l_desc=$'big_dir/\nsmall_dir/'
assert_eq "no recurse size sort desc with -L uses real dir size" "$(list_rel_raw "$NONREC_SIZE_L_ROOT" --sort size desc -R -L -d '*' | sed "s#^.*${NONREC_SIZE_L_ROOT}/##")" "$want_nonrec_size_l_desc"

# LONG EXTENDED MATRIX (-L)
LONG_ROOT="${TMP_BASE}/long_root"
mkdir -p "${LONG_ROOT}/folder_match/sub"
touch "${LONG_ROOT}/folder_match/a" "${LONG_ROOT}/folder_match/b" "${LONG_ROOT}/folder_match/sub/c"
long_out="$("$F" --timeout "$F_TIMEOUT" -L -d folder "$LONG_ROOT" 2>/dev/null)"
assert_regex "extended long format" "$long_out" '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2} [0-9]+([.][0-9]+)? ?(B|KiB|MiB|GiB|TiB) [0-9]+ .+/$'
assert_contains "extended long file count value" "$long_out" " 3 "
long_out_alias="$("$F" --timeout "$F_TIMEOUT" --long-true-dirsize -d folder "$LONG_ROOT" 2>/dev/null)"
assert_regex "extended long alias format" "$long_out_alias" '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2} [0-9]+([.][0-9]+)? ?(B|KiB|MiB|GiB|TiB) [0-9]+ .+/$'

LONG_SYM_ROOT="${TMP_BASE}/long_sym_root"
mkdir -p "${LONG_SYM_ROOT}/real_dir"
dd if=/dev/zero of="${LONG_SYM_ROOT}/real_dir/blob" bs=1024 count=1024 status=none
ln -s "${LONG_SYM_ROOT}/real_dir" "${LONG_SYM_ROOT}/sym_dir"
long_sym_out="$("$F" --timeout "$F_TIMEOUT" -L sym_dir "$LONG_SYM_ROOT" 2>/dev/null)"
assert_regex "extended long symlink dir not traversed" "$long_sym_out" '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2} [0-9]+([.][0-9]+)? ?(B|KiB|MiB|GiB|TiB) 0 .+/sym_dir$'

# IMPLICIT NAME CONTAINS-ALL + PATH MATRIX
CONTENT_ROOT="${TMP_BASE}/content_root"
mkdir -p "${CONTENT_ROOT}/folder1" "${CONTENT_ROOT}/relroot/inner"
touch "${CONTENT_ROOT}/folder1/alpha_beta_doc.txt"
touch "${CONTENT_ROOT}/relroot/inner/alpha_beta_gamma_hit.txt"
touch "${CONTENT_ROOT}/relroot/inner/alpha_only_miss.txt"
mkdir -p "${CONTENT_ROOT}/alpha_beta"
touch "${CONTENT_ROOT}/alpha_beta/alpha_beta_note.txt"
mkdir -p "${CONTENT_ROOT}/config_folder/fish_folder"
touch "${CONTENT_ROOT}/config_folder/fish_folder/index.html"

assert_eq "contains-all names with explicit absolute path arg" "$(list_rel "$CONTENT_ROOT" alpha beta gamma)" "relroot/inner/alpha_beta_gamma_hit.txt"
assert_eq "contains-all names with implicit relative path arg containing slash" "$(cd "$CONTENT_ROOT" && "$F" --timeout "$F_TIMEOUT" alpha beta gamma relroot/inner 2>/dev/null | sort)" "relroot/inner/alpha_beta_gamma_hit.txt"
assert_eq "contains-all names implicit mode works with two terms" "$(list_rel "$CONTENT_ROOT" alpha beta)" $'alpha_beta/\nalpha_beta/alpha_beta_note.txt\nfolder1/alpha_beta_doc.txt\nrelroot/inner/alpha_beta_gamma_hit.txt'
assert_eq "contains-all names treat bare folder token as search term" "$(cd "$CONTENT_ROOT" && "$F" --timeout "$F_TIMEOUT" alpha beta folder1 2>/dev/null | sort)" ""
assert_eq "contains-all names --path enables bare relative folder path" "$(cd "$CONTENT_ROOT" && "$F" --timeout "$F_TIMEOUT" alpha beta --path folder1 2>/dev/null | sort)" "folder1/alpha_beta_doc.txt"
assert_eq "contains-all flag forces name mode for two terms" "$(cd "$CONTENT_ROOT" && "$F" --timeout "$F_TIMEOUT" --contains-all alpha beta --path folder1 2>/dev/null | sort)" "folder1/alpha_beta_doc.txt"
contains_full_out="$(cd "$CONTENT_ROOT" && "$F" --timeout "$F_TIMEOUT" -F alpha beta 2>/dev/null | sort)"
assert_contains "contains-all -F keeps file under matching directory" "$contains_full_out" "alpha_beta/alpha_beta_note.txt"
cf_full_out="$(cd "$CONTENT_ROOT" && "$F" --timeout "$F_TIMEOUT" -F config fish 2>/dev/null | sort)"
assert_not_contains "contains-all -F excludes parent-only term matches in basename" "$cf_full_out" "config_folder/fish_folder/index.html"

echo "PASS: help matrix suite"
