fn main() {
    for o in sigil_chronos::sync::run_library() { println!("{}", o.summary()); }
}
