fn main() {
    let src_dir = std::path::Path::new("src");
    let parser_path = src_dir.join("parser.c");

    let mut config = cc::Build::new();
    config.std("c11").include(src_dir).file(&parser_path);

    #[cfg(target_env = "msvc")]
    config.flag("-utf-8");

    config.compile("tree-sitter-jags");
    println!("cargo:rerun-if-changed={}", parser_path.display());
    for header in ["alloc.h", "array.h", "parser.h"] {
        println!(
            "cargo:rerun-if-changed={}",
            src_dir.join("tree_sitter").join(header).display()
        );
    }
}
