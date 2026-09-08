fn main() {
    println!(
        "reccursive {} (api v{})",
        env!("CARGO_PKG_VERSION"),
        reccursive_protocol::API_VERSION
    );
}
