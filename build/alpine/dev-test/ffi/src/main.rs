extern "C" {
    fn answer() -> i32;
}

fn main() {
    println!("ffi-ok {}", unsafe { answer() });
}
