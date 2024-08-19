#[cfg(feature = "gitver")]
fn main() -> shadow_rs::SdResult<()> {
    println!("cargo::rerun-if-changed=src");
    shadow_rs::new()
}

#[cfg(not(feature = "gitver"))]
fn main() {}
