//! Package-owned native observer; bundled beside every Marsh application.

fn main() {
    if let Err(error) = marsh_instrument::run_tracer_helper() {
        eprintln!("marsh-trace: {error}");
        std::process::exit(1);
    }
}
