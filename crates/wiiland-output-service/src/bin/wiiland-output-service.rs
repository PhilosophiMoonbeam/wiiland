fn main() {
    if let Err(error) = wiiland_output_service::run_service() {
        eprintln!("WiiLandOutput could not start: {error}");
        std::process::exit(1);
    }
}
