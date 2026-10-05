use std::hint::black_box;
use std::time::Instant;

fn main() {
    let start = Instant::now();
    let mut sum = 0u64;
    for i in 0..100_000u32 {
        sum = sum.wrapping_add(toolbox::fib(black_box(i % 90)));
    }
    let elapsed = start.elapsed();
    println!(
        "bench fib: sum={} ns/iter={}",
        black_box(sum),
        elapsed.as_nanos() / 100_000
    );
}
