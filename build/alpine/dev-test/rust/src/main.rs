fn add(a: u32, b: u32) -> u32 {
    a + b
}

fn main() {
    println!("rust-ok {}", add(40, 2));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(40, 2), 42);
    }
}
