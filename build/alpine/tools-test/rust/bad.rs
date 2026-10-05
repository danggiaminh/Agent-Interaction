// Checked by clippy-driver and rustfmt, never built: clippy must flag it, rustfmt must want to rewrite it.
pub fn  needless( v : &Vec<u32> )->usize{
    let mut n=0;
    for i in 0..v.len() { if v[i]==1 { n+=1 } }
    return n;
}
