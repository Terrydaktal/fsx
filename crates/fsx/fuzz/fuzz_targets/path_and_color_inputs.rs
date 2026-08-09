#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let text = String::from_utf8_lossy(input);
    let _ = fsx::normalize_lexical(std::path::Path::new(text.as_ref()));
    let colors = fsx::colors::parse_ls_colors_value(text.as_ref());
    let _ = fsx::colors::color_code_for_path(text.as_ref(), false, false, false, false, &colors);
    let _ = fsx::terminal::escape_terminal_text(text.as_ref());
});
