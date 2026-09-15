use core::arch::asm;

pub struct TscClock {
    ticks_per_second: u64,
}

impl TscClock {
    pub fn calibrate() -> Self {
        let tps = cpuid_15_tps().unwrap_or(3_000_000_000);
        Self {
            ticks_per_second: tps.max(1_000_000),
        }
    }

    // Task 4 drives deadlines and polling; allow dead here so the Task 3
    // boot gate stays warning-free while keeping the specified API.
    #[allow(dead_code)]
    pub fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }

    pub fn now_ticks(&self) -> u64 {
        // RDTSC read; invariant-TSC assumed per CPUID 0x80000007:8 check done by caller platform code where required.
        unsafe {
            let low: u32;
            let high: u32;
            asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
            ((high as u64) << 32) | low as u64
        }
    }

    #[allow(dead_code)]
    pub fn deadline_secs(&self, secs: u64) -> relay_core::xhci::Deadline {
        relay_core::xhci::Deadline(
            self.now_ticks()
                .saturating_add(secs.saturating_mul(self.ticks_per_second)),
        )
    }

    #[allow(dead_code)]
    pub fn expired(&self, deadline: relay_core::xhci::Deadline) -> bool {
        self.now_ticks() >= deadline.0
    }
}

impl relay_core::xhci::Clock for TscClock {
    fn now_ticks(&self) -> u64 {
        TscClock::now_ticks(self)
    }

    fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }
}

fn cpuid_15_tps() -> Option<u64> {
    let result = core::arch::x86_64::__cpuid(0x15);
    if result.ebx == 0 || result.ecx == 0 {
        return None;
    }
    let crystal = result.ecx as u64;
    let num = result.ebx as u64;
    let den = result.eax as u64;
    if den == 0 {
        return None;
    }
    crystal.checked_mul(num)?.checked_div(den)
}
