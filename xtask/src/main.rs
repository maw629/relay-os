fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("doctor") if args.next().is_none() => doctor(),
        Some("image") => image(args),
        Some("verify-image") => verify_image(args),
        Some("qemu") => qemu(args),
        Some(command) => unavailable(&format!(
            "unknown command `{command}`; available commands are `doctor`, `image`, and `verify-image`"
        )),
        None => unreachable!(),
    }
}

fn image(mut args: impl Iterator<Item = String>) {
    let Some(flag) = args.next() else {
        unavailable("image requires `--output PATH`");
    };
    let Some(output) = args.next() else {
        unavailable("image requires `--output PATH`");
    };
    if flag != "--output" || args.next().is_some() {
        unavailable("image requires exactly `--output PATH`");
    }
    if let Err(error) = relay_xtask::image::build(std::path::Path::new(&output)) {
        eprintln!("cargo xtask image: {error}");
        std::process::exit(1);
    }
}

fn verify_image(mut args: impl Iterator<Item = String>) {
    let Some(image) = args.next() else {
        unavailable("verify-image requires IMAGE");
    };
    if args.next().is_some() {
        unavailable("verify-image requires exactly one IMAGE");
    }
    match relay_xtask::verify::verify_image(std::path::Path::new(&image)) {
        Ok(report) => {
            print!("{}{}", report.sgdisk, report.e2fsck);
        }
        Err(error) => {
            eprintln!("cargo xtask verify-image: {error}");
            std::process::exit(1);
        }
    }
}

fn qemu(mut args: impl Iterator<Item = String>) {
    let Some(action) = args.next() else {
        unavailable("qemu requires `boot|xhci IMAGE --display none --accel tcg`");
    };
    let Some(image) = args.next() else {
        unavailable("qemu boot requires IMAGE");
    };
    let Some(display_flag) = args.next() else {
        unavailable("qemu boot requires `--display none --accel tcg`");
    };
    let Some(display) = args.next() else {
        unavailable("qemu boot requires `--display none --accel tcg`");
    };
    let Some(accel_flag) = args.next() else {
        unavailable("qemu boot requires `--display none --accel tcg`");
    };
    let Some(accel) = args.next() else {
        unavailable("qemu boot requires `--display none --accel tcg`");
    };
    if (action != "boot" && action != "xhci")
        || display_flag != "--display"
        || accel_flag != "--accel"
        || args.next().is_some()
    {
        unavailable("qemu requires exactly `boot|xhci IMAGE --display none --accel tcg`");
    }
    if action == "xhci" {
        qemu_xhci(&image, &display, &accel);
        return;
    }
    let mut run = match relay_xtask::qemu::QemuRun::boot(
        std::path::Path::new(&image),
        &display,
        &accel,
        std::time::Duration::from_secs(20),
    ) {
        Ok(run) => run,
        Err(error) => {
            eprintln!("cargo xtask qemu boot: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = run.wait_for_marker(relay_xtask::qemu::KERNEL_ENTRY_MARKER) {
        eprintln!("cargo xtask qemu boot: {error}");
        eprintln!("{}", run.serial_log());
        std::process::exit(1);
    }
    print!("{}", run.serial_log());
}

fn qemu_xhci(image: &str, display: &str, accel: &str) {
    let mut run = match relay_xtask::qemu::QemuRun::xhci(
        std::path::Path::new(image),
        display,
        accel,
        std::time::Duration::from_secs(30),
    ) {
        Ok(run) => run,
        Err(error) => {
            eprintln!("cargo xtask qemu xhci: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = run.wait_for_marker(relay_xtask::qemu::XHCI_PROBE_MARKER) {
        eprintln!("cargo xtask qemu xhci: {error}");
        eprintln!("{}", run.serial_log());
        std::process::exit(1);
    }
    if let Err(error) = run.wait_for_marker("control_probe=8") {
        eprintln!("cargo xtask qemu xhci: {error}");
        eprintln!("{}", run.serial_log());
        std::process::exit(1);
    }
    print!("{}", run.serial_log());
}

fn doctor() {
    let report = relay_xtask::doctor::inspect();
    relay_xtask::doctor::print_versions();

    if !report.ready() {
        eprintln!(
            "missing required tools or targets: {}",
            report.missing.join(", ")
        );
        std::process::exit(1);
    }
}

fn unavailable(message: &str) -> ! {
    eprintln!("cargo xtask: {message}");
    std::process::exit(2);
}
