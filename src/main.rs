mod api;
mod config;
mod profile;

fn main() {
    if let Err(e) = config::ensure_dirs() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
    println!("codexctl ready");
}
