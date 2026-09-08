fn main() {
    println!(
        "{} {} (api v{})",
        reccursive_daemon::SERVICE_NAME,
        env!("CARGO_PKG_VERSION"),
        reccursive_daemon::api_version()
    );
}
