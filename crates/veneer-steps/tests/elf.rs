use std::path::Path;

use veneer_steps::mise::elf_interpreter;

#[test]
fn reads_pt_interp_of_dynamic_elf() {
    // A glibc aarch64 binary captured from a container run, when present.
    let gnu = Path::new("/tmp/rg-gnu");
    if gnu.is_file() {
        assert_eq!(
            elf_interpreter(gnu).as_deref(),
            Some("/lib/ld-linux-aarch64.so.1")
        );
    }
    // The static musl veneer binary has no loader.
    let static_bin = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/linux/release/veneer"
    ));
    if static_bin.is_file() {
        assert_eq!(elf_interpreter(static_bin), None);
    }
    // On Linux /bin/sh is a dynamic ELF whose loader exists; on macOS it is Mach-O (not ELF).
    let sh = elf_interpreter(Path::new("/bin/sh"));
    if cfg!(target_os = "linux") {
        let loader = sh.expect("/bin/sh is a dynamically linked ELF on Linux");
        assert!(Path::new(&loader).exists(), "{loader}");
    } else {
        assert_eq!(sh, None);
    }
}
