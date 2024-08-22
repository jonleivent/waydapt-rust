fn main() -> core::result::Result<(), Box<(dyn std::error::Error + 'static)>> {
    use vergen_gitcl::{BuildBuilder, CargoBuilder, Emitter, GitclBuilder, RustcBuilder};

    println!("cargo::rerun-if-changed=src");

    let build = BuildBuilder::default()
        .build_timestamp(true)
        .use_local(true)
        .build()?;
    let cargo = CargoBuilder::all_cargo()?;
    let gitcl = GitclBuilder::default()
        .all()
        .describe(true, true, None)
        .build()?;
    let rustc = RustcBuilder::all_rustc()?;

    Emitter::default()
        .add_instructions(&build)?
        .add_instructions(&cargo)?
        .add_instructions(&gitcl)?
        .add_instructions(&rustc)?
        .emit()?;

    Ok(())
}
