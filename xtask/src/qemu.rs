use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};

use crate::qmp;

pub const KERNEL_ENTRY_MARKER: &str = "[relay] phase=kernel-entry status=ok";
pub const XHCI_PROBE_MARKER: &str = "[relay] phase=xhci-probe status=ok";

pub struct QemuRun {
    child: Child,
    deadline: Instant,
    serial: PathBuf,
    qmp: PathBuf,
    qmp_log: PathBuf,
    framebuffer: PathBuf,
}

impl QemuRun {
    pub fn boot_production_image(timeout: Duration) -> Result<Self, String> {
        let image = Path::new("target/qemu-boot-test.img");
        crate::image::build(image).map_err(|error| error.to_string())?;
        Self::boot(image, "none", "tcg", timeout)
    }

    pub fn boot(
        image: &Path,
        display: &str,
        accel: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        Self::boot_inner(image, display, accel, timeout, &[])
    }

    /// Boots `image` with a USB keyboard attached to the emulated xHCI
    /// controller so the kernel's default-pipe GET_DESCRIPTOR probe has a
    /// connected port to enumerate. Without an attached device every port
    /// reports CCS clear and the probe can only print `control_probe=none`.
    pub fn xhci(
        image: &Path,
        display: &str,
        accel: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        Self::boot_inner(image, display, accel, timeout, &["-device", "usb-kbd"])
    }

    fn boot_inner(
        image: &Path,
        display: &str,
        accel: &str,
        timeout: Duration,
        extra_devices: &[&str],
    ) -> Result<Self, String> {
        if !image.is_file() {
            return Err(format!("image does not exist: {}", image.display()));
        }
        if display != "none" || accel != "tcg" {
            return Err("QEMU boot requires `--display none --accel tcg`".into());
        }

        let artifacts = Path::new("target/qemu");
        fs::create_dir_all(artifacts)
            .map_err(|error| format!("create QEMU artifact directory: {error}"))?;
        let serial = artifacts.join("serial.log");
        let stderr = artifacts.join("stderr.log");
        let qmp = artifacts.join("qmp.sock");
        let qmp_log = artifacts.join("qmp.log");
        let framebuffer = artifacts.join("framebuffer.ppm");
        let debug = artifacts.join("qemu-debug.log");
        let vars = artifacts.join("OVMF_VARS.fd");
        for path in [&serial, &stderr, &qmp, &qmp_log, &framebuffer, &debug] {
            let _ = fs::remove_file(path);
        }

        let firmware = Prebuilt::fetch(Source::EDK2_STABLE202408_R1, artifacts.join("ovmf"))
            .map_err(|error| format!("fetch OVMF firmware: {error}"))?;
        let code = firmware.get_file(Arch::X64, FileType::Code);
        fs::copy(firmware.get_file(Arch::X64, FileType::Vars), &vars)
            .map_err(|error| format!("prepare writable OVMF variables: {error}"))?;

        let qmp_argument = format!("unix:{},server=on,wait=off", qmp.display());
        let code_argument = format!("if=pflash,format=raw,readonly=on,file={}", code.display());
        let vars_argument = format!("if=pflash,format=raw,file={}", vars.display());
        let image_argument = format!("format=raw,file={}", image.display());
        let serial_argument = format!("file:{}", serial.display());
        let child = Command::new("qemu-system-x86_64")
            .args(["-machine", "q35", "-device", "qemu-xhci,p2=2,p3=2"])
            .args(extra_devices)
            .args([
                "-accel",
                accel,
                "-display",
                display,
                "-no-reboot",
                "-drive",
                &code_argument,
                "-drive",
                &vars_argument,
                "-drive",
                &image_argument,
                "-serial",
                &serial_argument,
                "-qmp",
                &qmp_argument,
                "-d",
                "int,cpu_reset",
                "-D",
                debug.to_str().ok_or("QEMU debug log path is not UTF-8")?,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&stderr).map_err(|error| {
                format!("create QEMU stderr log {}: {error}", stderr.display())
            })?))
            .spawn()
            .map_err(|error| format!("could not start qemu-system-x86_64: {error}"))?;

        Ok(Self {
            child,
            deadline: Instant::now() + timeout,
            serial,
            qmp,
            qmp_log,
            framebuffer,
        })
    }

    pub fn wait_for_marker(&mut self, marker: &str) -> Result<(), String> {
        while Instant::now() < self.deadline {
            let serial = self.serial_log();
            if serial.contains(marker) {
                self.capture_diagnostics();
                return Ok(());
            }
            let status = match self.child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    self.capture_diagnostics();
                    return Err(error.to_string());
                }
            };
            if let Some(status) = status {
                self.capture_diagnostics();
                return Err(format!("QEMU exited before marker `{marker}`: {status}"));
            }
            thread::sleep(Duration::from_millis(50));
        }
        self.capture_diagnostics();
        Err(format!("timed out waiting for serial marker `{marker}`"))
    }

    pub fn serial_log(&self) -> String {
        let mut serial = String::new();
        if let Ok(mut file) = File::open(&self.serial) {
            let _ = file.read_to_string(&mut serial);
        }
        serial
    }

    fn capture_diagnostics(&self) {
        let qmp_result = qmp::screendump(&self.qmp, &self.framebuffer);
        let _ = fs::write(&self.qmp_log, qmp_result.unwrap_or_else(|error| error));
    }
}

pub fn validate_serial_log(serial: &str) -> Result<(), String> {
    serial
        .contains(KERNEL_ENTRY_MARKER)
        .then_some(())
        .ok_or_else(|| format!("serial log lacks marker `{KERNEL_ENTRY_MARKER}`"))
}

/// Checks that a requested screen region contains at least one pixel different from the stable
/// lower-right background sample. It proves QEMU captured visible content near the boot banner,
/// while host `Mirror` tests remain responsible for exact byte-for-byte output semantics.
pub fn ppm_region_has_foreground(
    ppm: &[u8],
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<bool, String> {
    let mut offset = 0;
    if ppm_token(ppm, &mut offset)? != b"P6" {
        return Err("framebuffer screenshot is not a binary PPM".into());
    }
    let image_width = ppm_number(ppm_token(ppm, &mut offset)?)?;
    let image_height = ppm_number(ppm_token(ppm, &mut offset)?)?;
    if ppm_number(ppm_token(ppm, &mut offset)?)? != 255 {
        return Err("framebuffer screenshot does not use 8-bit PPM channels".into());
    }
    if offset >= ppm.len() || !ppm[offset].is_ascii_whitespace() {
        return Err("framebuffer screenshot has an invalid PPM header".into());
    }
    offset += 1;
    let pixel_len = image_width
        .checked_mul(image_height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or("framebuffer screenshot dimensions overflow")?;
    let pixels = ppm
        .get(
            offset
                ..offset
                    .checked_add(pixel_len)
                    .ok_or("framebuffer screenshot overflows")?,
        )
        .ok_or("framebuffer screenshot is truncated")?;
    if x >= image_width || y >= image_height || width == 0 || height == 0 {
        return Err("banner region is outside the framebuffer screenshot".into());
    }
    let end_x = x
        .checked_add(width)
        .ok_or("banner region overflows")?
        .min(image_width);
    let end_y = y
        .checked_add(height)
        .ok_or("banner region overflows")?
        .min(image_height);
    let background_start = (image_height - 1)
        .checked_mul(image_width)
        .and_then(|row| row.checked_add(image_width - 1))
        .and_then(|pixel| pixel.checked_mul(3))
        .ok_or("framebuffer screenshot dimensions overflow")?;
    let background = &pixels[background_start..background_start + 3];
    for row in y..end_y {
        for column in x..end_x {
            let start = (row * image_width + column) * 3;
            if &pixels[start..start + 3] != background {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn ppm_token<'a>(ppm: &'a [u8], offset: &mut usize) -> Result<&'a [u8], String> {
    loop {
        while ppm.get(*offset).is_some_and(u8::is_ascii_whitespace) {
            *offset += 1;
        }
        if ppm.get(*offset) != Some(&b'#') {
            break;
        }
        while ppm
            .get(*offset)
            .is_some_and(|byte| *byte != b'\n' && *byte != b'\r')
        {
            *offset += 1;
        }
    }
    let start = *offset;
    while ppm
        .get(*offset)
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        *offset += 1;
    }
    ppm.get(start..*offset)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "framebuffer screenshot has an incomplete PPM header".into())
}

fn ppm_number(token: &[u8]) -> Result<usize, String> {
    core::str::from_utf8(token)
        .map_err(|_| "framebuffer screenshot has a non-UTF-8 PPM header".to_owned())?
        .parse()
        .map_err(|_| "framebuffer screenshot has an invalid PPM dimension".to_owned())
}

impl Drop for QemuRun {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
