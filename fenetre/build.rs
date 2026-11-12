//! On Windows: program icon and information (publisher, description).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut r = winresource::WindowsResource::new();
        r.set_icon("../installateur/icone.ico");
        r.set("ProductName", "CNz");
        r.set("FileDescription", "CNz — compression gratuite de CN Technology");
        r.set("CompanyName", "CN Technology");
        r.set("LegalCopyright", "CN Technology — MIT OU Apache-2.0");
        r.compile().expect("ressources Windows");
    }
}
