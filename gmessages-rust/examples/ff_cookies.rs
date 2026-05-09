fn main() {
    env_logger::init();
    match gmessages_rust::cookies::read_default_firefox_cookies() {
        Ok(cookies) => {
            println!("Found {} cookies:", cookies.len());
            for k in cookies.keys() {
                println!("  - {k}");
            }
        }
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }
}
