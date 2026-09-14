fn main() {
    if let Err(error) = shelllist_search::serve() {
        eprintln!("shelllist-search: {error}");
        std::process::exit(1);
    }
}
