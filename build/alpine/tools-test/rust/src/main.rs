//! CPU burner: the program the profilers and debuggers look at.
use std::hint::black_box;

#[inline(never)]
fn burn(rounds: u64) -> u64 {
    let mut acc = 0u64;
    for i in 0..rounds {
        acc = acc.wrapping_add(toolbox::fib(black_box((i % 80) as u32)));
    }
    acc
}

fn main() {
    let rounds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    println!("burn {}", burn(rounds));
}
