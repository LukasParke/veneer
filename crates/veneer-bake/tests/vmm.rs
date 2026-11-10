use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn launchers_are_valid_posix_sh() {
    for (name, body) in veneer_bake::launchers() {
        if !name.ends_with(".sh") {
            assert!(body.contains("init=/sbin/veneer-init"));
            continue;
        }
        let mut child = Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(body.contains("root=/dev/vda rw"), "{name}");
        assert!(body.contains("${VENEER_CMDLINE:-}"), "{name}");
    }
}

#[test]
fn ovf_envelope_is_well_formed_and_complete() {
    let x = veneer_bake::ovf("agent<env>", "a-disk1.vmdk", 1234, 4 << 30, 2, 2048);
    assert!(x.starts_with("<?xml"));
    for needle in [
        "ovf:href=\"a-disk1.vmdk\" ovf:size=\"1234\"",
        "ovf:capacity=\"4294967296\"",
        "vmdk.html#streamOptimized",
        "<rasd:ResourceType>17</rasd:ResourceType>",
        "<rasd:HostResource>ovf:/disk/vmdisk1</rasd:HostResource>",
        "<Name>agent&lt;env&gt;</Name>",
    ] {
        assert!(x.contains(needle), "{needle}");
    }
    // Balanced tags (cheap well-formedness check without an XML dependency).
    let opens = x.matches("<Item>").count();
    assert_eq!(opens, x.matches("</Item>").count());
    assert_eq!(opens, 5);
}
