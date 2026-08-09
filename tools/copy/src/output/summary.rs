use crate::domain::FileRelationBreakdown;

pub(crate) fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, ch) in chars.iter().enumerate() {
        out.push(*ch);
        let rem = chars.len() - i - 1;
        if rem > 0 && rem.is_multiple_of(3) {
            out.push(',');
        }
    }
    out
}

pub(crate) fn count_col_width(label: &str, values: &[u64]) -> usize {
    let value_width = values
        .iter()
        .map(|v| format_number(*v).len())
        .max()
        .unwrap_or(1);
    value_width.max(label.len())
}

pub(crate) fn print_counts_table(
    file_row: Option<(u64, u64, u64, u64, u64, u64)>,
    dir_row: Option<(u64, u64, u64, u64, u64, u64)>,
) {
    let mut new_vals = Vec::new();
    let mut mod_vals = Vec::new();
    let mut ident_vals = Vec::new();
    let mut uncol_vals = Vec::new();
    let mut del_src_vals = Vec::new();
    let mut del_dst_vals = Vec::new();

    for row in [file_row, dir_row].into_iter().flatten() {
        new_vals.push(row.0);
        mod_vals.push(row.1);
        ident_vals.push(row.2);
        uncol_vals.push(row.3);
        del_src_vals.push(row.4);
        del_dst_vals.push(row.5);
    }

    let type_w = 5usize;
    let new_w = count_col_width("New", &new_vals);
    let mod_w = count_col_width("Mod", &mod_vals);
    let ident_w = count_col_width("Ident", &ident_vals);
    let uncol_w = count_col_width("Uncol", &uncol_vals);
    let del_src_w = count_col_width("Del(src)", &del_src_vals);
    let del_dst_w = count_col_width("Del(dest)", &del_dst_vals);

    println!(
        "{:<type_w$} | {:>new_w$} | {:>mod_w$} | {:>ident_w$} | {:>uncol_w$} | {:>del_src_w$} | {:>del_dst_w$}",
        "Type",
        "New",
        "Mod",
        "Ident",
        "Uncol",
        "Del(src)",
        "Del(dest)",
    );
    if let Some((new_v, mod_v, ident_v, uncol_v, del_src_v, del_dst_v)) = file_row {
        println!(
            "{:<type_w$} | {:>new_w$} | {:>mod_w$} | {:>ident_w$} | {:>uncol_w$} | {:>del_src_w$} | {:>del_dst_w$}",
            "Files",
            format_number(new_v),
            format_number(mod_v),
            format_number(ident_v),
            format_number(uncol_v),
            format_number(del_src_v),
            format_number(del_dst_v),
        );
    }
    if let Some((new_v, mod_v, ident_v, uncol_v, del_src_v, del_dst_v)) = dir_row {
        println!(
            "{:<type_w$} | {:>new_w$} | {:>mod_w$} | {:>ident_w$} | {:>uncol_w$} | {:>del_src_w$} | {:>del_dst_w$}",
            "Dirs",
            format_number(new_v),
            format_number(mod_v),
            format_number(ident_v),
            format_number(uncol_v),
            format_number(del_src_v),
            format_number(del_dst_v),
        );
    }
}

pub(crate) fn print_preview_counts_table(
    file_row: Option<(u64, u64, u64, u64, u64, u64)>,
    dir_row: Option<(u64, u64, u64, u64, u64, u64)>,
    breakdown: FileRelationBreakdown,
) {
    let mut new_vals = Vec::new();
    let mut uncol_vals = Vec::new();
    let mut del_src_vals = Vec::new();
    let mut del_dst_vals = Vec::new();
    let mut time_eq_size_eq_vals = Vec::new();
    let mut time_eq_src_gt_vals = Vec::new();
    let mut time_eq_src_lt_vals = Vec::new();
    let mut size_eq_new_vals = Vec::new();
    let mut size_eq_old_vals = Vec::new();
    let mut old_src_lt_vals = Vec::new();
    let mut old_src_gt_vals = Vec::new();
    let mut new_src_lt_vals = Vec::new();
    let mut new_src_gt_vals = Vec::new();

    for row in [file_row, dir_row].into_iter().flatten() {
        new_vals.push(row.0);
        uncol_vals.push(row.3);
        del_src_vals.push(row.4);
        del_dst_vals.push(row.5);
    }
    time_eq_size_eq_vals.push(breakdown.same_time_same_size);
    time_eq_src_gt_vals.push(breakdown.same_time_source_larger);
    time_eq_src_lt_vals.push(breakdown.same_time_source_smaller);
    size_eq_new_vals.push(breakdown.same_size_source_newer);
    size_eq_old_vals.push(breakdown.same_size_source_older);
    old_src_lt_vals.push(breakdown.source_older_smaller);
    old_src_gt_vals.push(breakdown.source_older_larger);
    new_src_lt_vals.push(breakdown.source_newer_smaller);
    new_src_gt_vals.push(breakdown.source_newer_larger);
    time_eq_size_eq_vals.push(0);
    time_eq_src_gt_vals.push(0);
    time_eq_src_lt_vals.push(0);
    size_eq_new_vals.push(0);
    size_eq_old_vals.push(0);
    old_src_lt_vals.push(0);
    old_src_gt_vals.push(0);
    new_src_lt_vals.push(0);
    new_src_gt_vals.push(0);

    let type_w = 5usize;
    let new_w = count_col_width("New", &new_vals);
    let uncol_w = count_col_width("Uncol", &uncol_vals);
    let del_src_w = count_col_width("Del(src)", &del_src_vals);
    let del_dst_w = count_col_width("Del(dest)", &del_dst_vals);
    let time_eq_size_eq_w = count_col_width("Time=Size=", &time_eq_size_eq_vals);
    let time_eq_src_gt_w = count_col_width("Time=Size+", &time_eq_src_gt_vals);
    let time_eq_src_lt_w = count_col_width("Time=Size-", &time_eq_src_lt_vals);
    let size_eq_new_w = count_col_width("Time+Size=", &size_eq_new_vals);
    let size_eq_old_w = count_col_width("Time-Size=", &size_eq_old_vals);
    let old_src_lt_w = count_col_width("Time-Size-", &old_src_lt_vals);
    let old_src_gt_w = count_col_width("Time-Size+", &old_src_gt_vals);
    let new_src_lt_w = count_col_width("Time+Size-", &new_src_lt_vals);
    let new_src_gt_w = count_col_width("Time+Size+", &new_src_gt_vals);

    println!(
        "{:<type_w$} | {:>new_w$} | {:>uncol_w$} | {:>time_eq_size_eq_w$} | {:>time_eq_src_gt_w$} | {:>time_eq_src_lt_w$} | {:>size_eq_new_w$}",
        "Type",
        "New",
        "Uncol",
        "Time=Size=",
        "Time=Size+",
        "Time=Size-",
        "Time+Size=",
    );
    if let Some((new_v, _mod_v, _ident_v, uncol_v, del_src_v, del_dst_v)) = file_row {
        println!(
            "{:<type_w$} | {:>new_w$} | {:>uncol_w$} | {:>time_eq_size_eq_w$} | {:>time_eq_src_gt_w$} | {:>time_eq_src_lt_w$} | {:>size_eq_new_w$}",
            "Files",
            format_number(new_v),
            format_number(uncol_v),
            format_number(breakdown.same_time_same_size),
            format_number(breakdown.same_time_source_larger),
            format_number(breakdown.same_time_source_smaller),
            format_number(breakdown.same_size_source_newer),
        );
        let _ = (del_src_v, del_dst_v);
    }
    if let Some((new_v, _mod_v, _ident_v, uncol_v, del_src_v, del_dst_v)) = dir_row {
        println!(
            "{:<type_w$} | {:>new_w$} | {:>uncol_w$} | {:>time_eq_size_eq_w$} | {:>time_eq_src_gt_w$} | {:>time_eq_src_lt_w$} | {:>size_eq_new_w$}",
            "Dirs",
            format_number(new_v),
            format_number(uncol_v),
            "0",
            "0",
            "0",
            "0",
        );
        let _ = (del_src_v, del_dst_v);
    }

    println!();
    println!(
        "{:<type_w$} | {:>size_eq_old_w$} | {:>old_src_lt_w$} | {:>old_src_gt_w$} | {:>new_src_lt_w$} | {:>new_src_gt_w$} | {:>del_src_w$} | {:>del_dst_w$}",
        "Type",
        "Time-Size=",
        "Time-Size-",
        "Time-Size+",
        "Time+Size-",
        "Time+Size+",
        "Del(src)",
        "Del(dest)",
    );
    if let Some((_new_v, _mod_v, _ident_v, _uncol_v, del_src_v, del_dst_v)) = file_row {
        println!(
            "{:<type_w$} | {:>size_eq_old_w$} | {:>old_src_lt_w$} | {:>old_src_gt_w$} | {:>new_src_lt_w$} | {:>new_src_gt_w$} | {:>del_src_w$} | {:>del_dst_w$}",
            "Files",
            format_number(breakdown.same_size_source_older),
            format_number(breakdown.source_older_smaller),
            format_number(breakdown.source_older_larger),
            format_number(breakdown.source_newer_smaller),
            format_number(breakdown.source_newer_larger),
            format_number(del_src_v),
            format_number(del_dst_v),
        );
    }
    if let Some((_new_v, _mod_v, _ident_v, _uncol_v, del_src_v, del_dst_v)) = dir_row {
        println!(
            "{:<type_w$} | {:>size_eq_old_w$} | {:>old_src_lt_w$} | {:>old_src_gt_w$} | {:>new_src_lt_w$} | {:>new_src_gt_w$} | {:>del_src_w$} | {:>del_dst_w$}",
            "Dirs",
            "0",
            "0",
            "0",
            "0",
            "0",
            format_number(del_src_v),
            format_number(del_dst_v),
        );
    }
    println!();
}
