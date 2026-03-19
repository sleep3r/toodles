use criterion::{black_box, criterion_group, criterion_main, Criterion};

/// Original unoptimized implementation
fn escape_html_original(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// New optimized implementation
fn escape_html_optimized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn bench_escape_html(c: &mut Criterion) {
    let mut group = c.benchmark_group("Escape HTML");

    let text_no_escapes = "This is a normal string without any special characters that need to be escaped.";
    let text_some_escapes = "This string has <some> tags and an & character.";
    let text_many_escapes = "<<<&>>>".repeat(10);

    group.bench_function("Original (No Escapes)", |b| {
        b.iter(|| escape_html_original(black_box(text_no_escapes)))
    });
    group.bench_function("Optimized (No Escapes)", |b| {
        b.iter(|| escape_html_optimized(black_box(text_no_escapes)))
    });

    group.bench_function("Original (Some Escapes)", |b| {
        b.iter(|| escape_html_original(black_box(text_some_escapes)))
    });
    group.bench_function("Optimized (Some Escapes)", |b| {
        b.iter(|| escape_html_optimized(black_box(text_some_escapes)))
    });

    group.bench_function("Original (Many Escapes)", |b| {
        b.iter(|| escape_html_original(black_box(&text_many_escapes)))
    });
    group.bench_function("Optimized (Many Escapes)", |b| {
        b.iter(|| escape_html_optimized(black_box(&text_many_escapes)))
    });

    group.finish();
}

criterion_group!(benches, bench_escape_html);
criterion_main!(benches);
