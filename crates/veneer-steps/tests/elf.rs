#[test]
fn reads_pt_interp_of_dynamic_elf() {
    let gnu = std::path::Path::new("/tmp/rg-gnu");
    if gnu.is_file() {
        assert_eq!(
            veneer_steps::mise::elf_interpreter(gnu).as_deref(),
            Some("/lib/ld-linux-aarch64.so.1")
        );
    }
    let static_bin = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/linux/release/veneer"
    ));
    if static_bin.is_file() {
        assert_eq!(veneer_steps::mise::elf_interpreter(static_bin), None);
    }
    assert_eq!(
        veneer_steps::mise::elf_interpreter(std::path::Path::new("/bin/sh")),
        None
    );
}
