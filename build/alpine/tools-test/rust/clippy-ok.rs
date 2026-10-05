pub fn ones(v: &[u32]) -> usize {
    v.iter().filter(|&&x| x == 1).count()
}
