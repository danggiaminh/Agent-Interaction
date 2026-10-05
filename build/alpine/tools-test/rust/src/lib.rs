//! Fixture library for the tools-layer tests.

/// Iterative Fibonacci (wrapping).
pub fn fib(n: u32) -> u64 {
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..n {
        let t = a.wrapping_add(b);
        a = b;
        b = t;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::fib;

    #[test]
    fn small_values() {
        assert_eq!(fib(0), 0);
        assert_eq!(fib(1), 1);
        assert_eq!(fib(10), 55);
    }

    #[test]
    fn large_value() {
        assert_eq!(fib(90), 2_880_067_194_370_816_120);
    }
}
