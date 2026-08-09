use criterion::{Criterion, black_box, criterion_group, criterion_main};
use fsx::{format_count, format_size_compact_3, metadata_snapshot_for, normalize_lexical};
use std::path::Path;

fn benchmark_primitives(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("fsx-primitives");
    group.bench_function("normalize-path", |bencher| {
        bencher.iter(|| normalize_lexical(black_box(Path::new("../../a/./b/../c"))))
    });
    group.bench_function("format-size", |bencher| {
        bencher.iter(|| format_size_compact_3(black_box(156_600_000)))
    });
    group.bench_function("format-count", |bencher| {
        bencher.iter(|| format_count(black_box(1_532_000)))
    });
    group.bench_function("metadata-snapshot", |bencher| {
        bencher.iter(|| metadata_snapshot_for(black_box(Path::new("Cargo.toml"))))
    });
    #[cfg(feature = "colors")]
    group.bench_function("ls-colors-lookup", |bencher| {
        let spec =
            fsx::colors::parse_ls_colors_value("di=01;34:ln=01;36:*.rs=38;5;208:ex=38;2;0;245;200");
        bencher.iter(|| {
            fsx::colors::color_code_for_path(
                black_box("src/main.rs"),
                false,
                false,
                false,
                false,
                &spec,
            )
        })
    });
    group.finish();
}

criterion_group!(benches, benchmark_primitives);
criterion_main!(benches);
