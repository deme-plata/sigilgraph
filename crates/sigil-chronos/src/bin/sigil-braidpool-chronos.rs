fn main() {
    for o in sigil_chronos::braidpool::run_library() { println!("{}", o.summary()); }
}
