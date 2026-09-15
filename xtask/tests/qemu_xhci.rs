use std::time::Duration;

#[test]
fn qemu_reports_xhci_probe_and_control_descriptor() {
    let image = std::path::Path::new("target/qemu-xhci-test.img");
    relay_xtask::image::build(image).expect("build disposable image");
    let mut run = relay_xtask::qemu::QemuRun::xhci(image, "none", "tcg", Duration::from_secs(30))
        .expect("start QEMU xhci scenario");
    run.wait_for_marker("[relay] phase=xhci-probe status=ok")
        .expect("xhci probe");
    run.wait_for_marker("control_probe=8")
        .expect("control descriptor probe");
    assert!(!run.serial_log().contains("status=Timeout"));
    assert!(!run.serial_log().contains("status=Stalled"));
}
