pub(crate) fn print_version_info() {
    if let Some(val) = option_env!("CARGO_PKG_VERSION") {
        println!("WAYDAPT SEMVER:      {val}");
    }

    if let Some(val) = option_env!("VERGEN_BUILD_TIMESTAMP") {
        println!("BUILD TIMESTAMP:     {val}");
    }

    if let Some(val) = option_env!("VERGEN_CARGO_DEBUG") {
        println!("CARGO DEBUG:         {val}");
    }
    if let Some(val) = option_env!("VERGEN_CARGO_DEPENDENCIES") {
        println!("CARGO DEPENDENCIES:  [{val}]");
    }
    if let Some(val) = option_env!("VERGEN_CARGO_FEATURES") {
        println!("CARGO FEATURES:      [{val}]");
    }
    if let Some(val) = option_env!("VERGEN_CARGO_OPT_LEVEL") {
        println!("CARGO OPT LEVEL:     {val}");
    }
    if let Some(val) = option_env!("VERGEN_CARGO_TARGET_TRIPLE") {
        println!("CARGO TARGET TRIPLE: {val}");
    }

    if let Some(val) = option_env!("VERGEN_GIT_DESCRIBE") {
        println!("GIT DESCRIBE:        {val}");
    }
    if let Some(val) = option_env!("VERGEN_GIT_BRANCH") {
        println!("GIT BRANCH:          {val}");
    }
    if let Some(val) = option_env!("VERGEN_GIT_COMMIT_DATE") {
        println!("GIT COMMIT DATE:     {val}");
    }
    if let Some(val) = option_env!("VERGEN_GIT_SHA") {
        println!("GIT SHA:             {val}");
    }

    if let Some(val) = option_env!("VERGEN_RUSTC_CHANNEL") {
        println!("RUSTC CHANNEL:       {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_COMMIT_DATE") {
        println!("RUSTC COMMIT DATE:   {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_COMMIT_HASH") {
        println!("RUSTC COMMIT HASH:   {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_HOST_TRIPLE") {
        println!("RUSTC HOST TRIPLE:   {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_LLVM_VERSION") {
        println!("RUSTC LLVM VERSION:  {val}");
    }
    if let Some(val) = option_env!("VERGEN_RUSTC_SEMVER") {
        println!("RUSTC SEMVER:        {val}");
    }
}
